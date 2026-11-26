//! One cluster's dashboard card: status, problem counts with the lists they open, CPU and memory
//! with sparklines. The set dashboard draws one per member and the fleet home screen one per
//! watched cluster.
//!
//! A card is built from a cluster's telemetry feed alone ([`ClusterRead`] to [`ClusterCard`]) and
//! assumes no cluster set. It keeps counts and sparklines, never the objects behind them. Clicks
//! are not dispatched here: the caller passes [`CardActions`] that name the action for each [`CardTarget`]
//! clicked.

// module-tests: set_dashboard_tests.rs

use std::rc::Rc;

use flint_core::KindId;
use flint_core::dashboard::{ProblemCategory, ProblemCounts};
use flint_core::metrics_trend::{TrendResource, TrendScope};
use flint_core::settings::ClusterTag;
use flint_core::telemetry::{Availability, ClusterTelemetry, MetricsCheck};
use flint_core::thresholds::{ThresholdLevel, ThresholdMetric, Thresholds};
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{Theme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;

use super::super::cluster_tag::tag_dot;
use super::super::registry;
use super::feed::{FeedId, ProblemLists, read};
use super::sparkline::sparkline;
use super::{
    DashboardStatus, METRICS_WAITING, MetricTile, MetricsPanel, ProblemTile, category_name,
};

/// The width of a card.
pub(in crate::ui) const CARD_WIDTH: f32 = 300.;
const SPARKLINE_HEIGHT: f32 = 24.;

/// What a click on a card opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui) enum CardTarget {
    /// The cluster itself: the card's name.
    Cluster,
    /// What a count counted.
    Problem {
        /// As [`super::CATEGORY_NAMES`] names it.
        category: &'static str,
        /// Which kind of short workload, when the count covers several.
        kind: Option<String>,
    },
}

/// The action a click on the card of the named cluster performs, if the caller offers one. A link
/// is drawn only where the card may open something and an action is named; the click goes
/// through the action registry from the card's focus.
pub(in crate::ui) type CardActions = Rc<dyn Fn(&str, CardTarget) -> Option<Box<dyn Action>>>;

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
            Self::Waiting => Some(METRICS_WAITING.to_owned()),
        }
    }
}

/// One cluster, as the workspace found it just now.
pub struct ClusterRead {
    pub cluster: String,
    /// Its exact display name.
    pub shown: String,
    pub tag: Option<ClusterTag>,
    /// Why it may not be read, if so. Its feed is then not consulted.
    pub blocked: Option<String>,
    pub telemetry: Option<ClusterTelemetry>,
    pub server: String,
    pub thresholds: Thresholds,
}

/// CPU or memory on a card.
#[derive(Clone, Debug, PartialEq)]
pub struct CardMetric {
    pub tile: MetricTile,
    /// Against this member's own threshold.
    pub level: ThresholdLevel,
}

/// One cluster's card.
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterCard {
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
    pub(super) metrics_notice: Option<String>,
    /// The kinds its short-workloads count opens. Empty while they cannot be told apart.
    pub short_kinds: Vec<&'static str>,
    /// The feed these values came from: a click is refused once another is running.
    pub feed: Option<FeedId>,
}

impl ClusterCard {
    pub(super) fn note(&self) -> Option<String> {
        match (self.state.note(), &self.metrics_notice) {
            (Some(state), Some(metrics)) => Some(format!("{state} {metrics}")),
            (None, Some(metrics)) => Some(metrics.clone()),
            (state, None) => state,
        }
    }

    /// The card in one line, for accessibility and tests.
    pub fn description(&self) -> String {
        let counts: Vec<String> = self.tiles.iter().map(ProblemTile::description).collect();
        match self.note() {
            Some(note) => format!("{}: {note} {}", self.shown, counts.join(", ")),
            None => format!("{}: {}", self.shown, counts.join(", ")),
        }
    }

    /// Drops every value of the card, which now says `state`.
    pub(in crate::ui) fn take_down(&mut self, state: CardState) {
        self.state = state;
        for tile in &mut self.tiles {
            tile.count = None;
            tile.level = ThresholdLevel::Unknown;
        }
        self.counts = ProblemCounts::from_fn(|_| None);
        self.level = ThresholdLevel::Unknown;
        self.cpu = None;
        self.memory = None;
        self.metrics_notice = None;
        self.short_kinds.clear();
        self.feed = None;
    }
}

/// What a card must say instead of its values while its feed is in `availability`, if anything.
/// A feed that lost its cluster keeps its id on the aged values it serves.
pub(in crate::ui) fn unreadable_state(availability: &Availability) -> Option<CardState> {
    match availability {
        Availability::Checked | Availability::Partial => None,
        Availability::Starting => Some(CardState::Waiting),
        Availability::CannotBeChecked(_) => Some(CardState::Unavailable(
            "This cluster cannot be checked.".to_owned(),
        )),
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
    tile: MetricTile,
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
        tile,
        level: thresholds.evaluate(metric, percent),
    }
}

fn state_of(read: &ClusterRead, status: &DashboardStatus) -> CardState {
    if let Some(why) = &read.blocked {
        return CardState::Blocked(why.clone());
    }
    // Every admitted member runs a feed in either watch mode (#715).
    if read.telemetry.is_none() {
        return CardState::Waiting;
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

/// One cluster's card. The objects behind its counts are dropped here.
pub(in crate::ui) fn card(member: ClusterRead, now: Timestamp) -> ClusterCard {
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
    let (cpu, memory, metrics_notice) = match (usable, model.metrics) {
        (None, _) => (None, None, None),
        (Some(_), MetricsPanel::Waiting) => (None, None, Some(METRICS_WAITING.to_owned())),
        (Some(_), MetricsPanel::NotInstalled) => {
            (None, None, Some("No metrics-server installed.".to_owned()))
        }
        (
            Some(telemetry),
            MetricsPanel::Tiles {
                cpu,
                memory,
                notice,
            },
        ) => {
            let notice = notice.or_else(|| {
                (cpu.value.is_none() || memory.value.is_none()).then(|| {
                    match telemetry.snapshot.metrics {
                        MetricsCheck::Current { nodes: 0 } => "No node metrics were returned.",
                        _ => "Waiting for fresh metrics from all nodes.",
                    }
                    .to_owned()
                })
            });
            (
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
                notice,
            )
        }
    };
    let level = [cpu.as_ref(), memory.as_ref()]
        .into_iter()
        .flatten()
        .map(|metric| metric.level)
        .fold(counts.overall_level(&member.thresholds), Ord::max);
    ClusterCard {
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
        metrics_notice,
    }
}

pub(in crate::ui) fn level_color(level: ThresholdLevel, theme: &Theme) -> Hsla {
    match level {
        ThresholdLevel::Warning => theme.warning,
        ThresholdLevel::Unknown => theme.muted_foreground,
        ThresholdLevel::Healthy => theme.foreground,
    }
}

pub(in crate::ui) fn count_link(
    id: String,
    label: String,
    value: String,
    color: Hsla,
    action: Option<Box<dyn Action>>,
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
            .on_click(registry::on_click_dyn_in(focus.clone(), action))
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

/// Draws `card`. A click on its name or a count performs what `actions` names, from `focus`.
pub(in crate::ui) fn render_card(
    card: &ClusterCard,
    actions: &CardActions,
    focus: &FocusHandle,
    theme: &Theme,
) -> impl IntoElement {
    let cluster = &card.cluster;
    let opens = card.state.counts();
    let header = h_flex()
        .id(SharedString::from(format!("set-card-open-{cluster}")))
        .aria_label(card.shown.clone())
        .gap_1()
        .min_w_0()
        .font_weight(FontWeight::SEMIBOLD)
        .text_sm()
        .when_some(card.tag, |el, tag| el.child(tag_dot(cluster, tag, theme)))
        .child(div().truncate().child(card.shown.clone()))
        .when_some(
            opens
                .then(|| actions(cluster, CardTarget::Cluster))
                .flatten(),
            |el, action| {
                el.cursor_pointer()
                    .hover(|el| el.bg(theme.table_hover))
                    .on_click(registry::on_click_dyn_in(focus.clone(), action))
            },
        )
        .test_support();
    let rows = card.tiles.iter().flat_map(|tile| {
        let name = category_name(tile.category);
        let links_kinds =
            tile.category == ProblemCategory::ShortWorkloads && card.short_kinds.len() > 1;
        let target = (opens
            && tile.opens_list()
            && !links_kinds
            && (tile.category != ProblemCategory::ShortWorkloads || !card.short_kinds.is_empty()))
        .then(|| CardTarget::Problem {
            category: name,
            kind: (tile.category == ProblemCategory::ShortWorkloads)
                .then(|| card.short_kinds.first().map(|kind| (*kind).to_owned()))
                .flatten(),
        });
        let mut rows = vec![
            count_link(
                format!("set-card-{cluster}-{name}"),
                tile.label.to_owned(),
                tile.value(),
                level_color(tile.level, theme),
                target.and_then(|target| actions(cluster, target)),
                focus,
                theme,
            )
            .into_any_element(),
        ];
        if links_kinds && opens {
            rows.extend(card.short_kinds.iter().filter_map(|kind| {
                let label = kind_entry(kind).map_or(*kind, |(_, label)| label);
                let action = actions(
                    cluster,
                    CardTarget::Problem {
                        category: name,
                        kind: Some((*kind).to_owned()),
                    },
                )?;
                Some(
                    count_link(
                        format!("set-card-{cluster}-{name}-{kind}"),
                        format!("Open short {label}"),
                        String::new(),
                        theme.foreground,
                        Some(action),
                        focus,
                        theme,
                    )
                    .into_any_element(),
                )
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
        .when_some(card.note(), |el, note| {
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
