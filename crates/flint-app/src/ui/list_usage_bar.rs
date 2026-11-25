//! The geometry, colour level, sort order and hover text of a usage bar, as pure functions of a
//! row's raw amounts (CPU in nanocores, memory in bytes). The list draws what they say.
//!
//! A bar's fill is the usage, its end the limit, and a tick marks the request. Without a limit the
//! bar ends at the request, and use past it is overflow. Without either there is no bar. A node's
//! bar ends at its allocatable amount and its tick is what its pods request.

use std::cmp::Ordering;

use flint_core::metrics::{Share, share};

/// What a row's usage is compared with. `0` is not set: a zero request or limit gives nothing to
/// scale a bar to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::ui) struct Scale {
    request: u64,
    limit: u64,
}

impl Scale {
    pub(in crate::ui) fn new(request: Option<u64>, limit: Option<u64>) -> Self {
        Self {
            request: request.unwrap_or(0),
            limit: limit.unwrap_or(0),
        }
    }

    pub(in crate::ui) fn request(self) -> Option<u64> {
        (self.request > 0).then_some(self.request)
    }

    pub(in crate::ui) fn limit(self) -> Option<u64> {
        (self.limit > 0).then_some(self.limit)
    }

    /// What the bar ends at: the limit, else the request.
    fn end(self) -> Option<u64> {
        self.limit().or(self.request())
    }
}

/// How the bar is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::ui) enum BarLevel {
    Normal,
    /// At or past the Settings threshold.
    Warning,
    /// Past the limit, or past the request when there is no limit.
    Over,
}

/// What to draw: every number is a fraction of the bar's width, from 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(in crate::ui) struct Bar {
    pub fill: f32,
    /// Where the request is, if the bar ends at the limit and a request is set.
    pub tick: Option<f32>,
    pub level: BarLevel,
}

fn fraction(part: u64, whole: u64) -> f32 {
    (part as f64 / whole as f64).min(1.) as f32
}

/// The bar for `used` against `scale`, or `None` if there is nothing to scale it to.
/// `threshold` is the Settings percentage at which the bar turns to a warning.
pub(in crate::ui) fn bar(used: u64, scale: Scale, threshold: u32) -> Option<Bar> {
    let end = scale.end()?;
    let level = if used > end {
        BarLevel::Over
    } else if u128::from(used) * 100 >= u128::from(threshold) * u128::from(end) {
        BarLevel::Warning
    } else {
        BarLevel::Normal
    };
    // A tick at the end of the bar says nothing: only a request below a limit gets one.
    let tick = scale
        .limit()
        .and_then(|limit| scale.request().map(|request| fraction(request, limit)))
        .filter(|tick| *tick < 1.);
    Some(Bar {
        fill: fraction(used, end),
        tick,
        level,
    })
}

/// Where a row sorts: by its share of the limit, else of the request. Rows with neither come
/// after, by raw value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rank {
    Share { used: u64, end: u64 },
    Raw(u64),
}

fn rank(used: u64, scale: Scale) -> Rank {
    match scale.end() {
        Some(end) => Rank::Share { used, end },
        None => Rank::Raw(used),
    }
}

/// Orders two known readings, each `(used, scale)`, for a usage column. Rows with a share sort
/// before rows without whichever way the column sorts, and `descending` turns each group over.
pub(in crate::ui) fn compare(a: (u64, Scale), b: (u64, Scale), descending: bool) -> Ordering {
    let order = match (rank(a.0, a.1), rank(b.0, b.1)) {
        (Rank::Share { .. }, Rank::Raw(_)) => return Ordering::Less,
        (Rank::Raw(_), Rank::Share { .. }) => return Ordering::Greater,
        (Rank::Raw(a), Rank::Raw(b)) => a.cmp(&b),
        (Rank::Share { used: ua, end: ea }, Rank::Share { used: ub, end: eb }) => {
            // ua / ea against ub / eb, without division.
            (u128::from(ua) * u128::from(eb)).cmp(&(u128::from(ub) * u128::from(ea)))
        }
    };
    if descending { order.reverse() } else { order }
}

/// The wording of the amounts a bar compares with.
#[derive(Debug, Clone, Copy)]
pub(in crate::ui) struct Labels {
    pub request: &'static str,
    pub limit: &'static str,
}

/// For pods and workloads.
pub(in crate::ui) const POD_LABELS: Labels = Labels {
    request: "Request",
    limit: "Limit",
};

/// For nodes: the tick is what pods request and the end is what the node offers.
pub(in crate::ui) const NODE_LABELS: Labels = Labels {
    request: "Requested by pods",
    limit: "Allocatable",
};

/// The hover text of a bar: the exact usage, the request and the limit, and the percentages.
/// `format` writes an amount as the column does.
pub(in crate::ui) fn hover(
    used: u64,
    scale: Scale,
    labels: Labels,
    format: impl Fn(u64) -> String,
) -> String {
    let line = |label: &str, amount: Option<u64>| match amount {
        None => format!("{label}: none"),
        Some(amount) => match share(used, Some(amount)) {
            Share::Percent(percent) => format!("{label}: {} ({percent}% used)", format(amount)),
            _ => format!("{label}: {}", format(amount)),
        },
    };
    let mut text = format!(
        "Using {}\n{}\n{}",
        format(used),
        line(labels.request, scale.request()),
        line(labels.limit, scale.limit()),
    );
    if scale.limit().is_none() && scale.request().is_some() {
        text.push_str("\nNo limit: the bar ends at the request");
    }
    text
}

/// The hover of a reading whose request and limit are not known, which is not the same as none.
pub(in crate::ui) fn hover_without_budget(used: u64, format: impl Fn(u64) -> String) -> String {
    format!("Using {}\nRequest and limit: unknown", format(used))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scale(request: Option<u64>, limit: Option<u64>) -> Scale {
        Scale::new(request, limit)
    }

    #[test]
    fn a_limit_ends_the_bar_and_the_request_is_a_tick() {
        let bar = bar(50, scale(Some(25), Some(100)), 90).unwrap();
        assert_eq!(bar.fill, 0.5);
        assert_eq!(bar.tick, Some(0.25));
        assert_eq!(bar.level, BarLevel::Normal);
    }

    #[test]
    fn with_a_request_only_the_bar_ends_at_the_request_and_overflow_is_red() {
        let within = bar(50, scale(Some(100), None), 90).unwrap();
        assert_eq!((within.fill, within.tick), (0.5, None));
        let over = bar(150, scale(Some(100), None), 90).unwrap();
        assert_eq!(
            (over.fill, over.tick, over.level),
            (1., None, BarLevel::Over)
        );
    }

    #[test]
    fn with_neither_there_is_no_bar() {
        assert_eq!(bar(5, scale(None, None), 90), None);
        // A zero amount gives nothing to scale to.
        assert_eq!(bar(5, scale(Some(0), Some(0)), 90), None);
    }

    #[test]
    fn the_level_follows_the_threshold_and_turns_red_past_the_end() {
        let level = |used| bar(used, scale(None, Some(100)), 90).unwrap().level;
        assert_eq!(level(89), BarLevel::Normal);
        assert_eq!(level(90), BarLevel::Warning);
        assert_eq!(level(100), BarLevel::Warning);
        assert_eq!(level(101), BarLevel::Over);
    }

    #[test]
    fn a_request_above_the_limit_or_at_it_draws_no_tick_inside_the_bar() {
        assert_eq!(bar(1, scale(Some(100), Some(100)), 90).unwrap().tick, None);
        assert_eq!(bar(1, scale(Some(200), Some(100)), 90).unwrap().tick, None);
    }

    #[test]
    fn huge_amounts_do_not_overflow() {
        let bar = bar(u64::MAX, scale(None, Some(u64::MAX)), 100).unwrap();
        assert_eq!((bar.fill, bar.level), (1., BarLevel::Warning));
    }

    fn order(a: (u64, Scale), b: (u64, Scale), descending: bool) -> Ordering {
        compare(a, b, descending)
    }

    #[test]
    fn rows_sort_by_their_share_of_the_limit_else_the_request() {
        let half_of_limit = (50, scale(None, Some(100)));
        let third_of_request = (10, scale(Some(30), None));
        assert_eq!(
            order(third_of_request, half_of_limit, false),
            Ordering::Less
        );
        assert_eq!(
            order(third_of_request, half_of_limit, true),
            Ordering::Greater
        );
        // The limit wins over the request.
        let by_limit = (50, scale(Some(10), Some(100)));
        assert_eq!(order(by_limit, half_of_limit, false), Ordering::Equal);
    }

    #[test]
    fn rows_with_neither_come_after_by_raw_value_in_both_directions() {
        let ranked = (1, scale(Some(1000), None));
        let small = (5, Scale::default());
        let large = (9, Scale::default());
        for descending in [false, true] {
            assert_eq!(order(ranked, large, descending), Ordering::Less);
            assert_eq!(order(large, ranked, descending), Ordering::Greater);
        }
        assert_eq!(order(small, large, false), Ordering::Less);
        assert_eq!(order(small, large, true), Ordering::Greater);
    }

    #[test]
    fn a_share_beats_a_larger_raw_amount() {
        let busy_small = (90, scale(None, Some(100)));
        let idle_large = (900, scale(None, Some(10_000)));
        assert_eq!(order(idle_large, busy_small, false), Ordering::Less);
    }

    fn mi(amount: u64) -> String {
        format!("{amount}Mi")
    }

    #[test]
    fn the_hover_says_usage_request_limit_and_both_percentages() {
        let text = hover(150, scale(Some(100), Some(300)), POD_LABELS, mi);
        assert_eq!(
            text,
            "Using 150Mi\nRequest: 100Mi (150% used)\nLimit: 300Mi (50% used)"
        );
    }

    #[test]
    fn the_hover_says_no_limit_when_the_bar_ends_at_the_request() {
        let text = hover(150, scale(Some(100), None), POD_LABELS, mi);
        assert_eq!(
            text,
            "Using 150Mi\nRequest: 100Mi (150% used)\nLimit: none\nNo limit: the bar ends at the request"
        );
    }

    #[test]
    fn a_node_hover_names_allocatable_and_what_pods_request() {
        let text = hover(50, scale(Some(25), Some(100)), NODE_LABELS, mi);
        assert!(
            text.contains("Requested by pods: 25Mi (200% used)"),
            "{text}"
        );
        assert!(text.contains("Allocatable: 100Mi (50% used)"), "{text}");
    }
}
