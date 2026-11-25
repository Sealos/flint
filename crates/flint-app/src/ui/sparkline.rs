//! A small chart of a series with gaps, drawn as a min-max band with a line through it. A series
//! with more samples than the chart has pixels is downsampled ([`downsample`]), so that a short
//! spike stays visible at any window. [`SparklineShape`] is the pure geometry and text;
//! [`sparkline`] paints it and shows a readout of the bucket under the pointer. A gap in the data
//! stays a gap in the line, never a drop to zero.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::base::TestSupportExt as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};

/// How thick the line is.
const LINE_WIDTH: f32 = 1.5;
/// The side of the mark for a sample with no neighbour, which a line could not show.
const DOT_SIZE: f32 = 2.;
/// How opaque the band behind the line is.
const BAND_OPACITY: f32 = 0.3;

/// The samples of one bucket of a downsampled series.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bucket {
    /// The first sample in the bucket.
    pub first: usize,
    /// One past the last sample in the bucket.
    pub end: usize,
    pub min: u64,
    pub max: u64,
    /// The mean of the samples that are not gaps, rounded down.
    pub mean: u64,
}

/// How many buckets `samples` become on a chart `width` pixels wide: one per pixel, or one per
/// sample when there are fewer samples than pixels.
pub fn bucket_count(samples: usize, width: f32) -> usize {
    samples.min(width.max(0.) as usize)
}

/// The samples `first..end` that bucket `index` of `buckets` covers in a series of `samples`.
/// With `buckets <= samples` every bucket holds at least one sample, and together they cover all.
fn bucket_range(samples: usize, buckets: usize, index: usize) -> (usize, usize) {
    (index * samples / buckets, (index + 1) * samples / buckets)
}

/// `values` reduced to `buckets` buckets (at most one per sample), oldest first. A bucket whose
/// samples are all gaps is `None`: the gap stays a gap.
pub fn downsample(values: &[Option<u64>], buckets: usize) -> Vec<Option<Bucket>> {
    let buckets = buckets.min(values.len());
    (0..buckets)
        .map(|index| {
            let (first, end) = bucket_range(values.len(), buckets, index);
            let mut seen = values[first..end].iter().flatten().copied().peekable();
            seen.peek()?;
            let (mut min, mut max, mut sum, mut count) = (u64::MAX, 0, 0u128, 0u128);
            for value in seen {
                min = min.min(value);
                max = max.max(value);
                sum += u128::from(value);
                count += 1;
            }
            Some(Bucket {
                first,
                end,
                min,
                max,
                mean: (sum / count) as u64,
            })
        })
        .collect()
}

/// One bucket's vertical extent, with `x` and both ends in `0.0..=1.0` and `y` measured upward.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Band {
    pub x: f32,
    pub low: f32,
    pub high: f32,
}

/// What to draw for a series at a given width.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plot {
    /// The line through the bucket means, as runs of connected points `(x, y)`.
    pub runs: Vec<Vec<(f32, f32)>>,
    /// The buckets that hold more than one value.
    pub bands: Vec<Band>,
}

/// A series, oldest sample first, with `None` for each gap.
///
/// The vertical scale runs from zero to the largest value, so a flat series does not look like a
/// swing.
#[derive(Clone, Debug)]
pub struct SparklineShape {
    values: Vec<Option<u64>>,
    /// When each sample was polled, aligned with `values`; empty or `None` where unknown, which
    /// reads as `end` minus `step` for each sample after it.
    times: Vec<Option<Timestamp>>,
    /// When the newest sample was taken.
    end: Timestamp,
    /// The time between two samples.
    step: Duration,
    /// How a value reads in the readout.
    format: fn(u64) -> String,
}

impl PartialEq for SparklineShape {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values && self.end == other.end && self.step == other.step
    }
}

impl SparklineShape {
    /// The shape of `values`, oldest first, with `None` for each gap, and the `times` they were
    /// polled. Where a time is unknown the last sample was taken at `end` and each one `step`
    /// after the one before.
    pub fn new(
        values: Vec<Option<u64>>,
        times: Vec<Option<Timestamp>>,
        end: Timestamp,
        step: Duration,
        format: fn(u64) -> String,
    ) -> Self {
        Self {
            values,
            times,
            end,
            step,
            format,
        }
    }

    /// The line and bands at `buckets` buckets.
    pub fn plot(&self, buckets: usize) -> Plot {
        let top = self.values.iter().flatten().copied().max().unwrap_or(0);
        let scale = |value: u64| {
            if top == 0 {
                0.
            } else {
                (value as f64 / top as f64) as f32
            }
        };
        let buckets = downsample(&self.values, buckets);
        let last = buckets.len().saturating_sub(1).max(1) as f64;
        let mut plot = Plot::default();
        let mut run = Vec::new();
        for (index, bucket) in buckets.iter().enumerate() {
            let x = (index as f64 / last) as f32;
            match bucket {
                Some(bucket) => {
                    // Missed polls between two buckets stay a break in the line.
                    let before = index
                        .checked_sub(1)
                        .and_then(|before| buckets[before])
                        .is_none_or(|before| self.values[before.end - 1].is_none());
                    if (before || self.values[bucket.first].is_none()) && !run.is_empty() {
                        plot.runs.push(std::mem::take(&mut run));
                    }
                    run.push((x, scale(bucket.mean)));
                    if bucket.min < bucket.max {
                        plot.bands.push(Band {
                            x,
                            low: scale(bucket.min),
                            high: scale(bucket.max),
                        });
                    }
                }
                None if !run.is_empty() => plot.runs.push(std::mem::take(&mut run)),
                None => {}
            }
        }
        if !run.is_empty() {
            plot.runs.push(run);
        }
        plot
    }

    /// Whether there is nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.values.iter().all(Option::is_none)
    }

    /// The time of sample `index`.
    fn time_of(&self, index: usize) -> Timestamp {
        if let Some(Some(polled)) = self.times.get(index) {
            return *polled;
        }
        let behind = self.values.len().saturating_sub(1 + index);
        let step = SignedDuration::try_from(self.step).unwrap_or(SignedDuration::ZERO);
        let back = i32::try_from(behind)
            .ok()
            .and_then(|behind| step.checked_mul(behind))
            .unwrap_or(SignedDuration::ZERO);
        self.end.checked_sub(back).unwrap_or(self.end)
    }

    /// What to say about bucket `index` of `buckets`: its time, or time range, and its lowest and
    /// highest value. `None` if there is no such bucket.
    pub fn readout(&self, buckets: usize, index: usize, zone: &TimeZone) -> Option<String> {
        let buckets = buckets.min(self.values.len());
        if index >= buckets {
            return None;
        }
        let (first, end) = bucket_range(self.values.len(), buckets, index);
        let at = |sample: usize, pattern: &str| {
            self.time_of(sample)
                .to_zoned(zone.clone())
                .strftime(pattern)
                .to_string()
        };
        let when = if end - first == 1 {
            at(first, "%H:%M:%S")
        } else {
            format!(
                "{}\u{2013}{}",
                at(first, "%H:%M:%S"),
                at(end - 1, "%H:%M:%S")
            )
        };
        let present = || self.values[first..end].iter().flatten().copied();
        Some(match (present().min(), present().max()) {
            (Some(min), Some(max)) if min == max => format!("{when}  {}", (self.format)(min)),
            (Some(min), Some(max)) => format!(
                "{when}  min {}  max {}",
                (self.format)(min),
                (self.format)(max)
            ),
            _ => format!("{when}  no data"),
        })
    }
}

/// The width of the bounds the plot uses: inset so that a line at either extreme is not clipped
/// by half its width.
fn plot_width(bounds: Bounds<Pixels>) -> f32 {
    (f32::from(bounds.size.width) - 2. * LINE_WIDTH).max(0.)
}

/// The bucket under `x` pixels from the left edge of the bounds, of `buckets` over `width`.
fn bucket_at(x: f32, width: f32, buckets: usize) -> usize {
    if buckets <= 1 || width <= 0. {
        return 0;
    }
    let fraction = ((x - LINE_WIDTH) / width).clamp(0., 1.);
    (fraction * (buckets - 1) as f32).round() as usize
}

/// What the chart remembers between frames.
#[derive(Default)]
struct Hover {
    /// Where the chart was last painted, so that the pointer maps to a bucket.
    bounds: Rc<Cell<Bounds<Pixels>>>,
    /// Pixels from the left edge of the chart to the pointer, while it is over the chart.
    at: Option<f32>,
}

/// `shape` painted in `color`, filling the element's bounds, with a readout under the pointer.
pub fn sparkline(shape: Arc<SparklineShape>, color: Hsla) -> impl IntoElement {
    Sparkline { shape, color }
}

#[derive(IntoElement)]
struct Sparkline {
    shape: Arc<SparklineShape>,
    color: Hsla,
}

impl RenderOnce for Sparkline {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let hover = window.use_keyed_state("sparkline-hover", cx, |_, _| Hover::default());
        let (painted, at) = {
            let hover = hover.read(cx);
            (hover.bounds.clone(), hover.at)
        };
        let width = plot_width(painted.get());
        let buckets = bucket_count(self.shape.values.len(), width);
        let readout = at.and_then(|x| {
            let text =
                self.shape
                    .readout(buckets, bucket_at(x, width, buckets), &TimeZone::system())?;
            Some((text, x > f32::from(painted.get().size.width) / 2.))
        });
        let theme = cx.theme().clone();
        let shape = self.shape;
        let color = self.color;
        let on_move = hover.clone();
        div()
            .id("sparkline")
            .relative()
            .size_full()
            .on_mouse_move(move |event, _, cx| {
                on_move.update(cx, |hover, cx| {
                    let at = f32::from(event.position.x - hover.bounds.get().origin.x);
                    if hover.at != Some(at) {
                        hover.at = Some(at);
                        cx.notify();
                    }
                });
            })
            .on_hover(move |hovered, _, cx| {
                if !*hovered {
                    hover.update(cx, |hover, cx| {
                        hover.at = None;
                        cx.notify();
                    });
                }
            })
            .child(
                canvas(
                    move |bounds, _, _| painted.set(bounds),
                    move |bounds, _, window, _| paint(&shape, color, bounds, window),
                )
                .size_full(),
            )
            .when_some(readout, |el, (text, pointer_right)| {
                el.child(
                    div()
                        .id("sparkline-readout")
                        .aria_label(text.clone())
                        .absolute()
                        .top_0()
                        .when(pointer_right, |el| el.left_0())
                        .when(!pointer_right, |el| el.right_0())
                        .px_1()
                        .rounded(theme.radius)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .text_color(theme.popover_foreground)
                        .text_xs()
                        .max_w_full()
                        .child(text)
                        .test_support(),
                )
            })
    }
}

fn paint(shape: &SparklineShape, color: Hsla, bounds: Bounds<Pixels>, window: &mut Window) {
    let inset = LINE_WIDTH;
    let width = plot_width(bounds);
    let height = (f32::from(bounds.size.height) - 2. * inset).max(0.);
    let buckets = bucket_count(shape.values.len(), width);
    let plot = shape.plot(buckets);
    let x_of = |x: f32| bounds.origin.x + px(inset + x * width);
    let y_of = |y: f32| bounds.origin.y + px(inset + (1. - y) * height);
    // One band quad per bucket, as wide as the bucket, so that neighbours meet.
    let band_width = if buckets > 1 {
        (width / (buckets - 1) as f32).max(1.)
    } else {
        1.
    };
    for band in &plot.bands {
        let top = y_of(band.high);
        let bottom = y_of(band.low);
        window.paint_quad(fill(
            Bounds::new(
                point(x_of(band.x) - px(band_width / 2.), top),
                size(px(band_width), (bottom - top).max(px(1.))),
            ),
            color.opacity(BAND_OPACITY),
        ));
    }
    for run in &plot.runs {
        if let [(x, y)] = run.as_slice() {
            let half = px(DOT_SIZE / 2.);
            window.paint_quad(fill(
                Bounds::new(
                    point(x_of(*x) - half, y_of(*y) - half),
                    size(px(DOT_SIZE), px(DOT_SIZE)),
                ),
                color,
            ));
            continue;
        }
        let mut path = PathBuilder::stroke(px(LINE_WIDTH));
        for (index, (x, y)) in run.iter().enumerate() {
            let at = point(x_of(*x), y_of(*y));
            if index == 0 {
                path.move_to(at);
            } else {
                path.line_to(at);
            }
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Band, Bucket, SparklineShape, bucket_at, bucket_count, downsample};
    use jiff::tz::TimeZone;
    use std::time::Duration;

    fn plain(value: u64) -> String {
        value.to_string()
    }

    fn shape(values: &[Option<u64>]) -> SparklineShape {
        SparklineShape::new(
            values.to_vec(),
            Vec::new(),
            "2026-01-01T12:00:00Z".parse().unwrap(),
            Duration::from_secs(15),
            plain,
        )
    }

    fn series(values: &[u64]) -> Vec<Option<u64>> {
        values.iter().copied().map(Some).collect()
    }

    #[test]
    fn a_gap_splits_the_line_instead_of_dropping_to_zero() {
        let plot = shape(&[Some(5), Some(10), None, Some(10)]).plot(usize::MAX);
        assert_eq!(plot.runs.len(), 2);
        assert_eq!(plot.runs[0], vec![(0., 0.5), (1. / 3., 1.)]);
        assert_eq!(plot.runs[1], vec![(1., 1.)]);
    }

    #[test]
    fn the_scale_starts_at_zero() {
        let plot = shape(&[Some(90), Some(100)]).plot(usize::MAX);
        assert_eq!(plot.runs[0], vec![(0., 0.9), (1., 1.)]);
    }

    #[test]
    fn all_zero_values_lie_on_the_baseline() {
        let plot = shape(&[Some(0), Some(0)]).plot(usize::MAX);
        assert_eq!(plot.runs[0], vec![(0., 0.), (1., 0.)]);
    }

    #[test]
    fn no_samples_draw_nothing() {
        assert!(shape(&[]).is_empty());
        assert!(shape(&[None, None]).is_empty());
        assert!(shape(&[None, None]).plot(10).runs.is_empty());
    }

    #[test]
    fn a_single_sample_stays_in_range() {
        let plot = shape(&[Some(7)]).plot(usize::MAX);
        assert_eq!(plot.runs[0], vec![(0., 1.)]);
    }

    #[test]
    fn fewer_samples_than_pixels_stay_one_to_one() {
        let values = series(&[1, 2, 3]);
        assert_eq!(bucket_count(3, 200.), 3);
        let buckets = downsample(&values, 3);
        assert_eq!(buckets.len(), 3);
        assert!(buckets.iter().flatten().all(|b| b.min == b.max));
        assert!(shape(&values).plot(3).bands.is_empty());
    }

    #[test]
    fn samples_are_bucketed_one_per_pixel() {
        assert_eq!(bucket_count(1000, 200.), 200);
        assert_eq!(bucket_count(1000, 0.), 0);
        assert_eq!(bucket_count(1000, -5.), 0);
    }

    #[test]
    fn each_bucket_holds_its_min_max_and_mean() {
        let buckets = downsample(&series(&[1, 9, 4, 4, 2, 8]), 3);
        assert_eq!(
            buckets,
            vec![
                Some(Bucket {
                    first: 0,
                    end: 2,
                    min: 1,
                    max: 9,
                    mean: 5
                }),
                Some(Bucket {
                    first: 2,
                    end: 4,
                    min: 4,
                    max: 4,
                    mean: 4
                }),
                Some(Bucket {
                    first: 4,
                    end: 6,
                    min: 2,
                    max: 8,
                    mean: 5
                }),
            ]
        );
    }

    #[test]
    fn uneven_buckets_cover_every_sample_once() {
        let values = series(&(0..10).collect::<Vec<_>>());
        let buckets = downsample(&values, 3);
        let mut next = 0;
        for bucket in buckets.iter().flatten() {
            assert_eq!(bucket.first, next);
            assert!(bucket.end > bucket.first);
            next = bucket.end;
        }
        assert_eq!(next, 10);
    }

    #[test]
    fn a_short_spike_survives_in_the_band() {
        let mut values = vec![Some(10); 100];
        values[57] = Some(1000);
        let plot = shape(&values).plot(10);
        let tallest = plot.bands.iter().map(|b| b.high).fold(0., f32::max);
        assert_eq!(tallest, 1.);
        // The mean alone would have hidden it.
        assert!(plot.runs[0].iter().all(|(_, y)| *y < 0.2));
    }

    #[test]
    fn a_bucket_of_only_gaps_stays_a_gap() {
        let mut values = series(&[5, 5, 5, 5]);
        values.extend([None, None, None, None]);
        values.extend(series(&[5, 5, 5, 5]));
        let buckets = downsample(&values, 3);
        assert_eq!(buckets.len(), 3);
        assert!(buckets[0].is_some() && buckets[1].is_none() && buckets[2].is_some());
        assert_eq!(shape(&values).plot(3).runs.len(), 2);
    }

    #[test]
    fn a_gap_between_two_buckets_breaks_the_line() {
        let plot = shape(&[Some(10), None, None, Some(90)]).plot(2);
        assert_eq!(plot.runs.len(), 2);
    }

    #[test]
    fn a_bucket_with_some_gaps_uses_the_samples_it_has() {
        let buckets = downsample(&[Some(2), None, Some(6), None], 2);
        assert_eq!(buckets[0].map(|b| (b.min, b.max, b.mean)), Some((2, 2, 2)));
        assert_eq!(buckets[1].map(|b| (b.min, b.max, b.mean)), Some((6, 6, 6)));
    }

    #[test]
    fn bands_are_scaled_like_the_line() {
        let plot = shape(&series(&[2, 4, 10, 10])).plot(2);
        assert_eq!(
            plot.bands,
            vec![Band {
                x: 0.,
                low: 0.2,
                high: 0.4
            }]
        );
    }

    #[test]
    fn the_pointer_picks_the_bucket_under_it() {
        assert_eq!(bucket_at(0., 100., 11), 0);
        assert_eq!(bucket_at(1.5 + 50., 100., 11), 5);
        assert_eq!(bucket_at(500., 100., 11), 10);
        assert_eq!(bucket_at(5., 100., 1), 0);
    }

    #[test]
    fn the_readout_gives_the_time_range_and_the_extremes() {
        let utc = TimeZone::UTC;
        // 8 samples ending at 12:00:00, 15 s apart: 11:58:15 .. 12:00:00.
        let s = shape(&series(&[1, 9, 4, 4, 2, 8, 3, 3]));
        assert_eq!(
            s.readout(2, 0, &utc).as_deref(),
            Some("11:58:15\u{2013}11:59:00  min 1  max 9")
        );
        assert_eq!(
            s.readout(2, 1, &utc).as_deref(),
            Some("11:59:15\u{2013}12:00:00  min 2  max 8")
        );
        assert_eq!(s.readout(2, 2, &utc), None);
    }

    #[test]
    fn the_readout_of_one_sample_gives_its_second_and_value() {
        let s = shape(&series(&[7, 8]));
        assert_eq!(
            s.readout(2, 1, &TimeZone::UTC).as_deref(),
            Some("12:00:00  8")
        );
        assert_eq!(
            s.readout(2, 0, &TimeZone::UTC).as_deref(),
            Some("11:59:45  7")
        );
    }

    #[test]
    fn the_readout_of_a_gap_says_so() {
        let s = shape(&[Some(1), None]);
        assert_eq!(
            s.readout(2, 1, &TimeZone::UTC).as_deref(),
            Some("12:00:00  no data")
        );
    }

    #[test]
    fn the_readout_uses_the_time_each_sample_was_polled() {
        let end: jiff::Timestamp = "2026-01-01T12:00:00Z".parse().unwrap();
        // A poll that ran 3 s late: the first sample's own time, not 15 s before the next.
        let first: jiff::Timestamp = "2026-01-01T11:59:42Z".parse().unwrap();
        let s = SparklineShape::new(
            series(&[7, 8]),
            vec![Some(first), Some(end)],
            end,
            Duration::from_secs(15),
            plain,
        );
        assert_eq!(
            s.readout(2, 0, &TimeZone::UTC).as_deref(),
            Some("11:59:42  7")
        );
    }
}
