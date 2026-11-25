//! The rings of the detail pane's Usage section. [`RingSpec`] is the pure part: how full the ring
//! is, where the request tick sits and which tone it takes. [`usage_ring`] paints it.
//!
//! A full circle is the limit, or the request when there is no limit, or allocatable for a node.
//! Use past that turns the ring red. With neither a request nor a limit there is nothing to
//! compare with, so there is no ring.

use std::f32::consts::TAU;

use flint_core::metrics::{Share, share};
use flint_core::thresholds::{ThresholdLevel, ThresholdMetric, Thresholds};
use gpui_kit::component::{StyledExt as _, Theme};
use gpui_kit::*;

/// The side of the ring.
const RING_SIZE: f32 = 88.;
/// How thick the ring is.
const RING_WIDTH: f32 = 8.;
/// How thick the request tick is.
const TICK_WIDTH: f32 = 2.;
/// How far the tick reaches beyond the ring on each side.
const TICK_OVERHANG: f32 = 2.;
/// Straight pieces per full turn. Enough that no corner shows at this size.
const SEGMENTS_PER_TURN: usize = 96;

/// How a ring is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tone {
    /// Below the warning threshold.
    Normal,
    /// At or above the CPU or memory percent threshold of Settings.
    Warning,
    /// Past the limit (or the request, when there is no limit).
    Over,
}

/// What a ring shows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct RingSpec {
    /// How much of the circle use fills, `0.0..=1.0`. A ring past its end is full.
    pub fill: f32,
    /// Where the request ends, as a share of the circle. Only set when a limit is the end of the
    /// circle and the request is below it.
    pub tick: Option<f32>,
    pub tone: Tone,
}

impl RingSpec {
    /// The ring for `used` against `request` and `limit` (any unit, the same for all three), or
    /// `None` when there is nothing to compare with. `limit` is allocatable for a node.
    ///
    /// A zero amount counts as not set: a share of nothing is no share.
    pub(super) fn of(
        used: u64,
        request: Option<u64>,
        limit: Option<u64>,
        metric: ThresholdMetric,
        thresholds: &Thresholds,
    ) -> Option<Self> {
        let request = request.filter(|&request| request > 0);
        let limit = limit.filter(|&limit| limit > 0);
        let end = limit.or(request)?;
        let tone = if used > end {
            Tone::Over
        } else {
            let percent = match share(used, Some(end)) {
                Share::Percent(percent) => Some(u64::from(percent)),
                Share::NotSet | Share::ZeroBudget => None,
            };
            match thresholds.evaluate(metric, percent) {
                ThresholdLevel::Warning => Tone::Warning,
                ThresholdLevel::Healthy | ThresholdLevel::Unknown => Tone::Normal,
            }
        };
        let tick = match (limit, request) {
            (Some(limit), Some(request)) if request < limit => {
                Some((request as f64 / limit as f64) as f32)
            }
            _ => None,
        };
        Some(Self {
            fill: (used as f64 / end as f64).min(1.) as f32,
            tick,
            tone,
        })
    }
}

/// The points of an arc starting at the top of the circle and running clockwise for `fraction`
/// of a turn, in a frame whose origin is the circle's centre and whose y grows downward.
pub(super) fn arc_points(radius: f32, fraction: f32) -> Vec<(f32, f32)> {
    let fraction = fraction.clamp(0., 1.);
    let steps = ((fraction * SEGMENTS_PER_TURN as f32).ceil() as usize).max(1);
    (0..=steps)
        .map(|step| {
            let angle = TAU * fraction * step as f32 / steps as f32;
            (radius * angle.sin(), -radius * angle.cos())
        })
        .collect()
}

/// The two ends of the tick across the ring at `fraction` of a turn.
pub(super) fn tick_ends(inner: f32, outer: f32, fraction: f32) -> [(f32, f32); 2] {
    let angle = TAU * fraction.clamp(0., 1.);
    let at = |radius: f32| (radius * angle.sin(), -radius * angle.cos());
    [at(inner), at(outer)]
}

fn tone_color(tone: Tone, theme: &Theme) -> Hsla {
    match tone {
        Tone::Normal => theme.primary,
        Tone::Warning => theme.warning,
        Tone::Over => theme.danger,
    }
}

/// `spec` painted as a ring with `center` in its middle.
pub(super) fn usage_ring(spec: RingSpec, center: String, theme: &Theme) -> impl IntoElement {
    let fill = tone_color(spec.tone, theme);
    let track = theme.border;
    let tick_color = theme.foreground;
    let ring = canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let mid = point(
                bounds.origin.x + bounds.size.width / 2.,
                bounds.origin.y + bounds.size.height / 2.,
            );
            let radius = (f32::from(bounds.size.width.min(bounds.size.height)) - RING_WIDTH) / 2.;
            let at = |(x, y): (f32, f32)| point(mid.x + px(x), mid.y + px(y));
            let mut stroke = |points: Vec<(f32, f32)>, width: f32, color: Hsla| {
                let mut path = PathBuilder::stroke(px(width));
                for (index, p) in points.into_iter().enumerate() {
                    if index == 0 {
                        path.move_to(at(p));
                    } else {
                        path.line_to(at(p));
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            };
            stroke(arc_points(radius, 1.), RING_WIDTH, track);
            if spec.fill > 0. {
                stroke(arc_points(radius, spec.fill), RING_WIDTH, fill);
            }
            if let Some(tick) = spec.tick {
                let half = RING_WIDTH / 2. + TICK_OVERHANG;
                stroke(
                    tick_ends(radius - half, radius + half, tick).to_vec(),
                    TICK_WIDTH,
                    tick_color,
                );
            }
        },
    )
    .size_full();
    div()
        .relative()
        .flex_none()
        .size(px(RING_SIZE))
        .child(ring)
        .child(
            div()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .font_semibold()
                .child(center),
        )
}

#[cfg(test)]
mod tests {
    use super::{RingSpec, ThresholdMetric, Thresholds, Tone, arc_points, tick_ends};

    const METRIC: ThresholdMetric = ThresholdMetric::CpuPercent;

    fn spec(used: u64, request: Option<u64>, limit: Option<u64>) -> Option<RingSpec> {
        RingSpec::of(used, request, limit, METRIC, &Thresholds::default())
    }

    #[test]
    fn a_limit_ends_the_circle_and_the_request_is_a_tick() {
        let ring = spec(100, Some(200), Some(400)).unwrap();
        assert_eq!(ring.fill, 0.25);
        assert_eq!(ring.tick, Some(0.5));
        assert_eq!(ring.tone, Tone::Normal);
    }

    #[test]
    fn with_no_limit_the_request_ends_the_circle() {
        let ring = spec(100, Some(200), None).unwrap();
        assert_eq!(ring.fill, 0.5);
        assert_eq!(ring.tick, None);
    }

    #[test]
    fn use_past_the_request_with_no_limit_overflows_in_red() {
        let ring = spec(300, Some(200), None).unwrap();
        assert_eq!(ring.fill, 1.);
        assert_eq!(ring.tone, Tone::Over);
    }

    #[test]
    fn use_past_the_limit_is_red_and_full() {
        let ring = spec(500, Some(200), Some(400)).unwrap();
        assert_eq!(ring.fill, 1.);
        assert_eq!(ring.tone, Tone::Over);
    }

    #[test]
    fn use_exactly_at_the_limit_is_not_past_it() {
        let ring = spec(400, None, Some(400)).unwrap();
        assert_eq!(ring.tone, Tone::Warning);
    }

    #[test]
    fn use_past_a_request_below_the_limit_is_not_red() {
        let ring = spec(300, Some(200), Some(400)).unwrap();
        assert_eq!(ring.tone, Tone::Normal);
        assert_eq!(ring.fill, 0.75);
    }

    #[test]
    fn the_warning_follows_the_threshold_of_settings() {
        let strict = Thresholds::default().merged(&{
            let mut overrides = flint_core::thresholds::ThresholdOverrides::default();
            overrides.set(METRIC, Some(50)).unwrap();
            overrides
        });
        let at = |thresholds: &Thresholds, used| {
            RingSpec::of(used, None, Some(100), METRIC, thresholds)
                .unwrap()
                .tone
        };
        assert_eq!(at(&Thresholds::default(), 89), Tone::Normal);
        assert_eq!(at(&Thresholds::default(), 90), Tone::Warning);
        assert_eq!(at(&strict, 49), Tone::Normal);
        assert_eq!(at(&strict, 50), Tone::Warning);
    }

    #[test]
    fn memory_uses_its_own_threshold() {
        let mut overrides = flint_core::thresholds::ThresholdOverrides::default();
        overrides
            .set(ThresholdMetric::MemoryPercent, Some(10))
            .unwrap();
        let thresholds = Thresholds::default().merged(&overrides);
        let cpu = RingSpec::of(
            50,
            None,
            Some(100),
            ThresholdMetric::CpuPercent,
            &thresholds,
        );
        let memory = RingSpec::of(
            50,
            None,
            Some(100),
            ThresholdMetric::MemoryPercent,
            &thresholds,
        );
        assert_eq!(cpu.unwrap().tone, Tone::Normal);
        assert_eq!(memory.unwrap().tone, Tone::Warning);
    }

    #[test]
    fn nothing_to_compare_with_means_no_ring() {
        assert_eq!(spec(100, None, None), None);
        assert_eq!(spec(100, Some(0), Some(0)), None);
    }

    #[test]
    fn a_zero_limit_falls_back_to_the_request() {
        let ring = spec(100, Some(200), Some(0)).unwrap();
        assert_eq!(ring.fill, 0.5);
        assert_eq!(ring.tick, None);
    }

    #[test]
    fn a_request_at_the_limit_needs_no_tick() {
        assert_eq!(spec(100, Some(400), Some(400)).unwrap().tick, None);
    }

    #[test]
    fn a_node_ring_runs_to_allocatable() {
        let ring = spec(1000, None, Some(4000)).unwrap();
        assert_eq!(ring.fill, 0.25);
        assert_eq!(ring.tick, None);
    }

    #[test]
    fn a_full_arc_starts_at_the_top_and_closes_on_it() {
        let points = arc_points(10., 1.);
        let first = points[0];
        let last = *points.last().unwrap();
        assert!(first.0.abs() < 1e-4 && (first.1 + 10.).abs() < 1e-4);
        assert!((first.0 - last.0).abs() < 1e-3 && (first.1 - last.1).abs() < 1e-3);
    }

    #[test]
    fn a_quarter_arc_runs_clockwise_to_the_right() {
        let points = arc_points(10., 0.25);
        let last = *points.last().unwrap();
        assert!((last.0 - 10.).abs() < 1e-3 && last.1.abs() < 1e-3);
    }

    #[test]
    fn an_empty_arc_is_a_single_point_not_nothing() {
        assert_eq!(arc_points(10., 0.).len(), 2);
    }

    #[test]
    fn the_tick_crosses_the_ring_at_its_share() {
        let [inner, outer] = tick_ends(6., 14., 0.5);
        assert!(inner.0.abs() < 1e-4 && (inner.1 - 6.).abs() < 1e-4);
        assert!(outer.0.abs() < 1e-4 && (outer.1 - 14.).abs() < 1e-4);
    }
}
