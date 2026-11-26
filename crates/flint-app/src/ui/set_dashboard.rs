//! The dashboard of a running cluster set (#103, ADR-0017): a card per member, worst first, under
//! a totals strip.
//!
//! It reads only the telemetry the core already holds, through
//! [`flint_core::CoreHandle::member_telemetry`], and so adds no LIST, watch or feed. A member with
//! no usable feed shows why, counts as unknown and is never healthy, and a total that lacks a
//! member says so. A card keeps counts and two sparklines, never the objects behind them: those
//! are read from the member's feed when a count is clicked.

#[cfg(test)]
use std::cell::RefCell;
#[cfg(test)]
use std::collections::HashMap;
use std::rc::Rc;

use flint_core::dashboard::{ProblemCategory, ProblemTotals};
#[cfg(test)]
use flint_core::telemetry::Availability;
use flint_core::telemetry::ClusterTelemetry;
use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::{ActiveTheme as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::Timestamp;
use serde::Deserialize;

pub use super::cluster_card::CardState;
use super::cluster_card::{CardActions, CardTarget, card, count_link, render_card};
pub use super::cluster_card::{ClusterCard as SetCard, ClusterRead as MemberRead};
pub(in crate::ui) use super::cluster_card::{kind_entry, unreadable_state};
#[cfg(test)]
use super::feed::FeedId;
use super::feed::{ProblemLists, problem_lists};
use super::{CATEGORY_NAMES, NO_VALUE, category_name};

/// How often an open dashboard reads again with no change from any feed.
pub(in crate::ui) const AGE_OUT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(15);

/// Open the combined list filtered to one member.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Action)]
#[action(namespace = cluster_sets, no_json)]
#[serde(deny_unknown_fields)]
pub struct OpenSetMember {
    pub cluster: String,
    /// The [`SetDashboardView::drawn_feeds`] the link was drawn with.
    #[serde(default)]
    pub drawn: Option<u64>,
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
    /// The [`SetDashboardView::drawn_feeds`] the link was drawn with.
    #[serde(default)]
    pub drawn: Option<u64>,
}

/// The category [`OpenSetProblem`] names.
pub(in crate::ui) fn category_named(name: &str) -> Option<ProblemCategory> {
    ProblemCategory::ALL
        .into_iter()
        .zip(CATEGORY_NAMES)
        .find_map(|(category, named)| (named == name).then_some(category))
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

/// The one kind a total of short workloads counted, which its link opens.
fn total_kind(cards: &[SetCard], category: ProblemCategory) -> Option<&'static str> {
    if category != ProblemCategory::ShortWorkloads {
        return None;
    }
    cards
        .iter()
        .filter(|card| card.counts.get(category).is_some_and(|count| count > 0))
        .find_map(|card| card.short_kinds.first().copied())
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
    /// A member's feed state, `Checked` when absent.
    pub availability: HashMap<String, Availability>,
    /// Replaces a member's feed and lists right after its next read.
    pub replace_after_read: RefCell<Option<(String, FeedId, ProblemLists)>>,
    replaced: RefCell<HashMap<String, (FeedId, ProblemLists)>>,
}

#[cfg(test)]
impl TestFeeds {
    /// One read of `cluster`'s feed: its id, state and lists, all from the same moment.
    pub fn read(&self, cluster: &str) -> Option<(FeedId, Availability, Option<ProblemLists>)> {
        let replaced = self.replaced.borrow().get(cluster).cloned();
        let (feed, lists) = match replaced {
            Some((feed, lists)) => (feed, Some(lists)),
            None => (
                self.feeds.get(cluster)?.clone(),
                self.lists.get(cluster).cloned(),
            ),
        };
        let next = self
            .replace_after_read
            .borrow_mut()
            .take_if(|(member, ..)| member == cluster);
        if let Some((member, feed, lists)) = next {
            self.replaced.borrow_mut().insert(member, (feed, lists));
        }
        let availability = self
            .availability
            .get(cluster)
            .cloned()
            .unwrap_or(Availability::Checked);
        Some((feed, availability, lists))
    }
}

/// Draws a [`SetDashboardModel`]. The workspace feeds it, and answers its links.
pub struct SetDashboardView {
    pub(in crate::ui) model: SetDashboardModel,
    /// Moves whenever a card's feed changes, so a link drawn before that opens nothing.
    pub(in crate::ui) drawn_feeds: u64,
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
            drawn_feeds: 0,
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
            let feeds_moved = self.model.cards.len() != model.cards.len()
                || model.cards.iter().any(|card| {
                    self.model
                        .card(&card.cluster)
                        .is_none_or(|old| old.feed != card.feed)
                });
            if feeds_moved {
                self.drawn_feeds += 1;
            }
            self.model = model;
            cx.notify();
        }
    }

    /// Takes down the named cards now, and the totals they counted towards.
    pub(in crate::ui) fn take_down(
        &mut self,
        cards: Vec<(String, CardState)>,
        cx: &mut Context<Self>,
    ) {
        let mut next = self.model.cards.clone();
        for (cluster, state) in cards {
            if let Some(card) = next.iter_mut().find(|card| card.cluster == cluster) {
                card.take_down(state);
            }
        }
        self.set_model(assemble(next), cx);
    }
}

impl Render for SetDashboardView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let members = self.model.cards.len();
        let drawn = self.drawn_feeds;
        let focus = &self.focus;
        let open: CardActions = Rc::new(move |cluster, target| {
            Some(match target {
                CardTarget::Cluster => Box::new(OpenSetMember {
                    cluster: cluster.to_owned(),
                    drawn: Some(drawn),
                }),
                CardTarget::Problem { category, kind } => Box::new(OpenSetProblem {
                    category: category.to_owned(),
                    cluster: Some(cluster.to_owned()),
                    kind,
                    drawn: Some(drawn),
                }),
            })
        });
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
                            kind: total_kind(&self.model.cards, total.category).map(str::to_owned),
                            drawn: Some(drawn),
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
                                action.map(|action| Box::new(action) as Box<dyn Action>),
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
                            .map(|card| render_card(card, &open, focus, theme)),
                    )
                    .test_support(),
            )
            .test_support()
    }
}

#[cfg(test)]
#[path = "set_dashboard_tests.rs"]
mod tests;
