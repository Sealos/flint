//! The set dashboard's part of the visible view tree ([`crate::ui::view_state`]): its cards, worst
//! first, and the totals under them.

use flint_core::thresholds::ThresholdLevel;

use super::super::view_state::{SetCardState, SetCountState, SetDashboardState, SetTotalState};
use super::set::{CardState, SetCard, SetDashboardModel};
use super::{category_label, category_name};

fn level_name(level: ThresholdLevel) -> &'static str {
    match level {
        ThresholdLevel::Healthy => "healthy",
        ThresholdLevel::Unknown => "unknown",
        ThresholdLevel::Warning => "warning",
    }
}

fn state_name(card: &SetCard) -> &'static str {
    match card.state {
        CardState::Live => "live",
        CardState::Partial(_) => "partial",
        CardState::Waiting => "waiting",
        CardState::NotWatched => "not_watched",
        CardState::Unavailable(_) => "unavailable",
        CardState::Blocked(_) => "blocked",
    }
}

impl SetDashboardModel {
    pub(in crate::ui) fn view_state(&self) -> SetDashboardState {
        SetDashboardState {
            cards: self
                .cards
                .iter()
                .map(|card| SetCardState {
                    cluster: card.cluster.clone(),
                    shown: card.shown.clone(),
                    tag: card.tag,
                    state: state_name(card),
                    note: card.state.note(),
                    level: level_name(card.level),
                    counts: card
                        .tiles
                        .iter()
                        .map(|tile| SetCountState {
                            category: category_name(tile.category),
                            label: tile.label,
                            count: tile.count,
                            opens: tile.opens_list(),
                        })
                        .collect(),
                })
                .collect(),
            totals: self
                .totals
                .iter()
                .map(|total| SetTotalState {
                    category: category_name(total.category),
                    label: category_label(total.category),
                    value: total.value(self.cards.len()),
                    known: total.known,
                    missing: total.missing,
                    opens: total.opens,
                })
                .collect(),
            partial: self.partial.clone(),
        }
    }
}
