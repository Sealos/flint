//! The dashboard of a running cluster set (#103, ADR-0017): a card per member, worst first, under
//! a totals strip.
//!
//! It reads only the telemetry the core already holds, through
//! [`flint_core::CoreHandle::member_telemetry`], and so adds no LIST, watch or feed. A member with
//! no usable feed shows why, counts as unknown and is never healthy, and a total that lacks a
//! member says so. A card keeps counts and two sparklines, never the objects behind them: those
//! are read from the member's feed when a count is clicked.

#[cfg(test)]
use std::collections::HashMap;

use flint_core::KindId;
use flint_core::dashboard::{ProblemCategory, ProblemCounts, ProblemTotals};
use flint_core::metrics_trend::{TrendResource, TrendScope};
use flint_core::settings::ClusterTag;
use flint_core::telemetry::ClusterTelemetry;
use flint_core::thresholds::{ThresholdLevel, ThresholdMetric, Thresholds};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{ActiveTheme as _, Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;
use serde::Deserialize;

use super::super::cluster_tag::tag_dot;
use super::super::registry;
use super::feed::{FeedId, ProblemLists, problem_lists, read};
use super::sparkline::sparkline;
use super::{
    CATEGORY_NAMES, DashboardStatus, MetricTile, MetricsPanel, NO_VALUE, ProblemTile, category_name,
};

/// How often an open dashboard reads again with no change from any feed.
pub(in crate::ui) const AGE_OUT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);
const CARD_WIDTH: f32 = 300.;
const SPARKLINE_HEIGHT: f32 = 24.;

/// Open the combined list filtered to one member.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = cluster_sets, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenSetMember {
    pub cluster: String,
}

/// Open the combined list filtered to what a dashboard count counted.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = cluster_sets, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenSetProblem {
    /// As [`CATEGORY_NAMES`] names it.
    pub category: String,
    /// The member; every member the total counted when `None`.
    #[serde(default)]
    pub cluster: Option<String>,
    /// Which kind of short workload, when the count covers several.
    #[serde(default)]
    pub kind: Option<String>,
}

/// The category [`OpenSetProblem`] names.
pub(in crate::ui) fn category_named(name: &str) -> Option<ProblemCategory> {
    ProblemCategory::ALL
        .into_iter()
        .zip(CATEGORY_NAMES)
        .find_map(|(category, named)| (named == name).then_some(category))
}

/// The kind and label a problem list opens, as the navigation names them.
pub(in crate::ui) fn kind_entry(plural: &str) -> Option<(KindId, &'static str)> {
    let (group, plural, label) = match plural {
        "pods" => ("", "pods", "Pods"),
        "nodes" => ("", "nodes", "Nodes"),
        "events" => ("", "events", "Events"),
        "persistentvolumeclaims" => ("", "persistentvolumeclaims", "Persistent Volume Claims"),
        "jobs" => ("batch", "jobs", "Jobs"),
        "deployments" => ("apps", "deployments", "Deployments"),
        "statefulsets" => ("apps", "statefulsets", "Stateful Sets"),
        "daemonsets" => ("apps", "daemonsets", "Daemon Sets"),
        _ => return None,
    };
    Some((KindId::new(group, plural), label))
}

/// Where a member stands on the dashboard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardState {
    /// Every kind could be read.
    Live,
    /// Some kinds could not be read; the text names them.
    Partial(String),
    /// A feed should be running but has shown nothing yet.
    Waiting,
    /// Its watch mode runs no feed while it is in the background.
    NotWatched,
    /// Its feed cannot read the cluster now.
    Unavailable(String),
    /// Paused, moved, left out or needing sign-in: nothing of it may be read or opened.
    Blocked(String),
}

impl CardState {
    /// Whether the card's counts are this member's current values.
    pub fn counts(&self) -> bool {
        matches!(self, Self::Live | Self::Partial(_))
    }

    /// The state in a few words, when there is something to say.
    pub fn note(&self) -> Option<String> {
        match self {
            Self::Live => None,
            Self::Partial(why) | Self::Unavailable(why) | Self::Blocked(why) => Some(why.clone()),
            Self::Waiting => Some("Waiting for telemetry.".to_owned()),
            Self::NotWatched => {
                Some("Not watched in the background: its watch mode is On view.".to_owned())
            }
        }
    }
}

/// One member, as the workspace found it just now.
pub struct MemberRead {
    pub cluster: String,
    /// Its exact display name.
    pub shown: String,
    pub tag: Option<ClusterTag>,
    /// Why it may not be read, if so. Its feed is then not consulted.
    pub blocked: Option<String>,
    pub telemetry: Option<ClusterTelemetry>,
    pub server: String,
    pub thresholds: Thresholds,
    /// Its watch mode runs a feed in the background.
    pub watched: bool,
}

/// CPU or memory on a card.
#[derive(Clone, Debug, PartialEq)]
pub struct CardMetric {
    pub tile: MetricTile,
    /// Against this member's own threshold.
    pub level: ThresholdLevel,
}

/// One member's card.
#[derive(Clone, Debug, PartialEq)]
pub struct SetCard {
    pub cluster: String,
    pub shown: String,
    pub tag: Option<ClusterTag>,
    pub state: CardState,
    pub tiles: Vec<ProblemTile>,
    pub counts: ProblemCounts,
    /// Against this member's own thresholds.
    pub level: ThresholdLevel,
    pub cpu: Option<CardMetric>,
    pub memory: Option<CardMetric>,
    /// The kinds its short-workloads count opens. Empty while they cannot be told apart.
    pub short_kinds: Vec<&'static str>,
    /// The feed these values came from: a click is refused once another is running.
    pub feed: Option<FeedId>,
}

impl SetCard {
    /// The card in one line, for accessibility and tests.
    pub fn description(&self) -> String {
        let counts: Vec<String> = self.tiles.iter().map(ProblemTile::description).collect();
        match self.state.note() {
            Some(note) => format!("{}: {note} {}", self.shown, counts.join(", ")),
            None => format!("{}: {}", self.shown, counts.join(", ")),
        }
    }
}

/// One count across the set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TotalTile {
    pub category: ProblemCategory,
    pub label: &'static str,
    pub known: u32,
    /// Members this count could not include.
    pub missing: u32,
    /// Whether one list can show everything it counted.
    pub opens: bool,
}

impl TotalTile {
    /// The number as shown: an undercount says so, and a count of nothing shows no number.
    pub fn value(&self, members: usize) -> String {
        if self.missing as usize >= members {
            NO_VALUE.to_owned()
        } else if self.missing > 0 {
            format!("{}+", self.known)
        } else {
            self.known.to_string()
        }
    }

    /// The tile in one line, for accessibility and tests.
    pub fn description(&self, members: usize) -> String {
        let value = self.value(members);
        match self.missing {
            0 => format!("{}: {value}", self.label),
            missing => format!(
                "{}: {value}, partial: {missing} of {members} clusters not counted",
                self.label
            ),
        }
    }
}

/// Everything the view draws.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SetDashboardModel {
    /// Worst first; members with the same rank by name.
    pub cards: Vec<SetCard>,
    pub totals: Vec<TotalTile>,
    /// Names the members a total lacks.
    pub partial: Option<String>,
}

impl SetDashboardModel {
    /// The card of `cluster`.
    pub fn card(&self, cluster: &str) -> Option<&SetCard> {
        self.cards.iter().find(|card| card.cluster == cluster)
    }
}

/// How much of the allocatable `resource` the newest poll used, in percent.
fn used_percent(telemetry: &ClusterTelemetry, resource: TrendResource) -> Option<u64> {
    let allocatable = telemetry.snapshot.capacity.as_ref()?.allocatable();
    let whole = match resource {
        TrendResource::Cpu => allocatable.cpu?.nanos(),
        TrendResource::Memory => allocatable.memory?.bytes(),
    };
    let used = telemetry.with_trends(|trends| {
        trends
            .series(
                &telemetry.snapshot.cluster,
                TrendScope::Total,
                resource,
                Timestamp::MIN,
            )?
            .last()
            .flatten()
    })?;
    (whole > 0).then(|| (u128::from(used) * 100 / u128::from(whole)) as u64)
}

fn card_metric(
    telemetry: &ClusterTelemetry,
    tile: &MetricTile,
    resource: TrendResource,
    metric: ThresholdMetric,
    thresholds: &Thresholds,
) -> CardMetric {
    // A tile with no value has no current sample, so an old total must not set its level.
    let percent = tile
        .value
        .as_ref()
        .and_then(|_| used_percent(telemetry, resource));
    CardMetric {
        tile: tile.clone(),
        level: thresholds.evaluate(metric, percent),
    }
}

fn state_of(read: &MemberRead, status: &DashboardStatus) -> CardState {
    if let Some(why) = &read.blocked {
        return CardState::Blocked(why.clone());
    }
    if read.telemetry.is_none() {
        return match read.watched {
            true => CardState::Waiting,
            false => CardState::NotWatched,
        };
    }
    match status {
        DashboardStatus::Starting => CardState::Waiting,
        DashboardStatus::Checked => CardState::Live,
        DashboardStatus::Partial(why) => CardState::Partial(why.clone()),
        DashboardStatus::CannotBeChecked(why) => CardState::Unavailable(why.clone()),
    }
}

fn short_kinds(lists: &ProblemLists) -> Vec<&'static str> {
    lists
        .get(&ProblemCategory::ShortWorkloads)
        .map_or_else(Vec::new, |lists| {
            lists.iter().map(|list| list.kind).collect()
        })
}

/// One member's card. The objects behind its counts are dropped here.
fn card(member: MemberRead, now: Timestamp) -> SetCard {
    let telemetry = member
        .telemetry
        .as_ref()
        .filter(|_| member.blocked.is_none());
    let (model, lists) = read(
        telemetry,
        &member.cluster,
        Some(&member.server),
        &member.thresholds,
        now,
    );
    let state = state_of(&member, &model.status);
    let usable = telemetry.filter(|_| state.counts());
    let tiles: Vec<ProblemTile> = model
        .tiles
        .into_iter()
        .map(|tile| match usable {
            Some(_) => tile,
            None => ProblemTile {
                count: None,
                level: ThresholdLevel::Unknown,
                ..tile
            },
        })
        .collect();
    let counts = ProblemCounts::from_fn(|category| {
        tiles
            .iter()
            .find(|tile| tile.category == category)
            .and_then(|tile| tile.count)
    });
    let (cpu, memory) = match (usable, &model.metrics) {
        (Some(telemetry), MetricsPanel::Tiles { cpu, memory, .. }) => (
            Some(card_metric(
                telemetry,
                cpu,
                TrendResource::Cpu,
                ThresholdMetric::CpuPercent,
                &member.thresholds,
            )),
            Some(card_metric(
                telemetry,
                memory,
                TrendResource::Memory,
                ThresholdMetric::MemoryPercent,
                &member.thresholds,
            )),
        ),
        _ => (None, None),
    };
    let level = [cpu.as_ref(), memory.as_ref()]
        .into_iter()
        .flatten()
        .map(|metric| metric.level)
        .fold(counts.overall_level(&member.thresholds), Ord::max);
    SetCard {
        short_kinds: if usable.is_some() {
            short_kinds(&lists)
        } else {
            Vec::new()
        },
        feed: usable.map(FeedId::of),
        cluster: member.cluster,
        shown: member.shown,
        tag: member.tag,
        state,
        tiles,
        counts,
        level,
        cpu,
        memory,
    }
}

/// `cards` worst first, with what they add up to.
pub(in crate::ui) fn assemble(mut cards: Vec<SetCard>) -> SetDashboardModel {
    cards.sort_by(|a, b| {
        (a.counts.attention_rank(), &a.shown, &a.cluster).cmp(&(
            b.counts.attention_rank(),
            &b.shown,
            &b.cluster,
        ))
    });
    let sums: ProblemTotals = cards.iter().map(|card| &card.counts).collect();
    let totals = ProblemCategory::ALL
        .into_iter()
        .map(|category| TotalTile {
            category,
            label: super::category_label(category),
            known: sums.known(category),
            missing: sums.unknown_clusters(category),
            opens: total_opens(&cards, category),
        })
        .collect();
    let lacking: Vec<&str> = cards
        .iter()
        .filter(|card| !card.state.counts())
        .map(|card| card.shown.as_str())
        .collect();
    let partial = (!lacking.is_empty()).then(|| {
        format!(
            "Partial: {} of {} clusters not counted ({}).",
            lacking.len(),
            cards.len(),
            lacking.join(", ")
        )
    });
    SetDashboardModel {
        cards,
        totals,
        partial,
    }
}

/// Whether one list can show all a total counted: short workloads may span several kinds.
fn total_opens(cards: &[SetCard], category: ProblemCategory) -> bool {
    let mut counted = cards
        .iter()
        .filter(|card| card.counts.get(category).is_some())
        .peekable();
    if counted.peek().is_none() {
        return false;
    }
    if category != ProblemCategory::ShortWorkloads {
        return true;
    }
    let mut kinds: Vec<&str> = Vec::new();
    for card in counted.filter(|card| card.counts.get(category) != Some(0)) {
        if card.short_kinds.is_empty() {
            return false;
        }
        kinds.extend(card.short_kinds.iter().copied());
    }
    kinds.sort_unstable();
    kinds.dedup();
    kinds.len() <= 1
}

/// The dashboard of `members`. Runs off the UI thread.
pub(in crate::ui) fn build(members: Vec<MemberRead>, now: Timestamp) -> SetDashboardModel {
    assemble(
        members
            .into_iter()
            .map(|member| card(member, now))
            .collect(),
    )
}

/// The objects behind each count of `telemetry`, read when a count is clicked.
pub(in crate::ui) fn lists_now(telemetry: &ClusterTelemetry) -> ProblemLists {
    problem_lists(telemetry, Timestamp::now())
}

/// Stands in for the core's running feeds in a view test.
#[cfg(test)]
#[derive(Default)]
pub(in crate::ui) struct TestFeeds {
    pub feeds: HashMap<String, FeedId>,
    pub lists: HashMap<String, ProblemLists>,
}

/// Draws a [`SetDashboardModel`]. The workspace feeds it, and answers its links.
pub struct SetDashboardView {
    pub(in crate::ui) model: SetDashboardModel,
    focus: FocusHandle,
    scroll: ScrollHandle,
    #[cfg(test)]
    pub(in crate::ui) test_feeds: Option<TestFeeds>,
    /// A build is running.
    pub(in crate::ui) reading: bool,
    /// Something changed during a build: build again when it ends.
    pub(in crate::ui) dirty: bool,
    pub(in crate::ui) _read: Task<()>,
    pub(in crate::ui) _updates: Task<()>,
    pub(in crate::ui) _age_out: Task<()>,
}

impl Focusable for SetDashboardView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl SetDashboardView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            model: SetDashboardModel::default(),
            focus: cx.focus_handle(),
            scroll: ScrollHandle::new(),
            #[cfg(test)]
            test_feeds: None,
            reading: false,
            dirty: false,
            _read: Task::ready(()),
            _updates: Task::ready(()),
            _age_out: Task::ready(()),
        }
    }

    /// Shows `model`, redrawing only if it differs.
    pub fn set_model(&mut self, model: SetDashboardModel, cx: &mut Context<Self>) {
        if self.model != model {
            self.model = model;
            cx.notify();
        }
    }
}

fn level_color(level: ThresholdLevel, theme: &Theme) -> Hsla {
    match level {
        ThresholdLevel::Warning => theme.warning,
        ThresholdLevel::Unknown => theme.muted_foreground,
        ThresholdLevel::Healthy => theme.foreground,
    }
}

fn count_link(
    id: String,
    label: String,
    value: String,
    color: Hsla,
    action: Option<OpenSetProblem>,
    focus: &FocusHandle,
    theme: &Theme,
) -> impl IntoElement {
    let row = h_flex()
        .id(SharedString::from(id))
        .aria_label(label.clone())
        .justify_between()
        .gap_2()
        .px_1()
        .rounded_sm()
        .text_xs()
        .child(div().text_color(theme.muted_foreground).child(label))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(color)
                .child(value),
        );
    match action {
        Some(action) => row
            .cursor_pointer()
            .hover(|el| el.bg(theme.table_hover))
            .on_click(registry::on_click_in(focus.clone(), action))
            .test_support()
            .into_any_element(),
        None => row.test_support().into_any_element(),
    }
}

fn metric_row(id: String, metric: &CardMetric, theme: &Theme) -> impl IntoElement {
    v_flex()
        .id(SharedString::from(id))
        .aria_label(metric.tile.description())
        .flex_1()
        .min_w_0()
        .gap_0p5()
        .child(
            h_flex()
                .justify_between()
                .text_xs()
                .child(
                    div()
                        .text_color(theme.muted_foreground)
                        .child(metric.tile.label),
                )
                .child(
                    div()
                        .text_color(level_color(metric.level, theme))
                        .child(metric.tile.shown_value().to_owned()),
                ),
        )
        .child(
            div()
                .h(px(SPARKLINE_HEIGHT))
                .w_full()
                .child(sparkline(metric.tile.shape.clone(), theme.primary)),
        )
        .test_support()
}

fn render_card(card: &SetCard, focus: &FocusHandle, theme: &Theme) -> impl IntoElement {
    let cluster = &card.cluster;
    let opens = card.state.counts();
    let header = h_flex()
        .id(SharedString::from(format!("set-card-open-{cluster}")))
        .gap_1()
        .min_w_0()
        .font_weight(FontWeight::SEMIBOLD)
        .text_sm()
        .when_some(card.tag, |el, tag| el.child(tag_dot(cluster, tag, theme)))
        .child(div().truncate().child(card.shown.clone()))
        .when(opens, |el| {
            el.cursor_pointer()
                .hover(|el| el.bg(theme.table_hover))
                .on_click(registry::on_click_in(
                    focus.clone(),
                    OpenSetMember {
                        cluster: cluster.clone(),
                    },
                ))
        })
        .test_support();
    let rows = card.tiles.iter().flat_map(|tile| {
        let name = category_name(tile.category);
        let links_kinds =
            tile.category == ProblemCategory::ShortWorkloads && card.short_kinds.len() > 1;
        let action = (opens
            && tile.opens_list()
            && !links_kinds
            && (tile.category != ProblemCategory::ShortWorkloads || !card.short_kinds.is_empty()))
        .then(|| OpenSetProblem {
            category: name.to_owned(),
            cluster: Some(cluster.clone()),
            kind: None,
        });
        let mut rows = vec![
            count_link(
                format!("set-card-{cluster}-{name}"),
                tile.label.to_owned(),
                tile.value(),
                level_color(tile.level, theme),
                action,
                focus,
                theme,
            )
            .into_any_element(),
        ];
        if links_kinds && opens {
            rows.extend(card.short_kinds.iter().map(|kind| {
                let label = kind_entry(kind).map_or(*kind, |(_, label)| label);
                count_link(
                    format!("set-card-{cluster}-{name}-{kind}"),
                    format!("Open short {label}"),
                    String::new(),
                    theme.foreground,
                    Some(OpenSetProblem {
                        category: name.to_owned(),
                        cluster: Some(cluster.clone()),
                        kind: Some((*kind).to_owned()),
                    }),
                    focus,
                    theme,
                )
                .into_any_element()
            }));
        }
        rows
    });
    v_flex()
        .id(SharedString::from(format!("set-card-{cluster}")))
        .aria_label(card.description())
        .w(px(CARD_WIDTH))
        .flex_none()
        .gap_1()
        .p_3()
        .rounded(theme.radius)
        .border_1()
        .border_color(match card.level {
            ThresholdLevel::Warning => theme.warning,
            _ => theme.border,
        })
        .child(header)
        .when_some(card.state.note(), |el, note| {
            el.child(
                div()
                    .id(SharedString::from(format!("set-card-note-{cluster}")))
                    .aria_label(note.clone())
                    .text_xs()
                    .text_color(theme.warning)
                    .child(note)
                    .test_support(),
            )
        })
        .children(rows.collect::<Vec<_>>())
        .when(card.cpu.is_some() || card.memory.is_some(), |el| {
            el.child(
                h_flex()
                    .gap_2()
                    .children(
                        card.cpu
                            .as_ref()
                            .map(|m| metric_row(format!("set-card-cpu-{cluster}"), m, theme)),
                    )
                    .children(
                        card.memory
                            .as_ref()
                            .map(|m| metric_row(format!("set-card-memory-{cluster}"), m, theme)),
                    ),
            )
        })
        .test_support()
}

impl Render for SetDashboardView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let members = self.model.cards.len();
        let focus = &self.focus;
        v_flex()
            .id("set-dashboard")
            .track_focus(focus)
            .size_full()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .gap_3()
            .p_3()
            .child(
                h_flex()
                    .id("set-dashboard-totals")
                    .flex_wrap()
                    .gap_2()
                    .children(self.model.totals.iter().map(|total| {
                        let name = category_name(total.category);
                        let action = total.opens.then(|| OpenSetProblem {
                            category: name.to_owned(),
                            cluster: None,
                            kind: None,
                        });
                        div()
                            .id(SharedString::from(format!("set-total-tile-{name}")))
                            .aria_label(total.description(members))
                            .w(px(150.))
                            .p_1()
                            .rounded(theme.radius)
                            .border_1()
                            .border_color(theme.border)
                            .child(count_link(
                                format!("set-total-{name}"),
                                total.label.to_owned(),
                                total.value(members),
                                match (total.known, total.missing) {
                                    (0, 0) => theme.foreground,
                                    (0, _) => theme.muted_foreground,
                                    _ => theme.warning,
                                },
                                action,
                                focus,
                                theme,
                            ))
                    }))
                    .test_support(),
            )
            .when_some(self.model.partial.clone(), |el, partial| {
                el.child(
                    div()
                        .id("set-dashboard-partial")
                        .aria_label(partial.clone())
                        .text_xs()
                        .text_color(theme.warning)
                        .child(partial)
                        .test_support(),
                )
            })
            .child(
                h_flex()
                    .id("set-dashboard-cards")
                    .flex_wrap()
                    .items_start()
                    .gap_3()
                    .children(
                        self.model
                            .cards
                            .iter()
                            .map(|card| render_card(card, focus, theme)),
                    )
                    .test_support(),
            )
            .test_support()
    }
}

#[cfg(test)]
#[path = "set_dashboard_tests.rs"]
mod tests;
