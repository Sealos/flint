//! A small line chart of a series with gaps. [`SparklineShape`] is the pure geometry; [`sparkline`]
//! paints it. A gap in the data stays a gap in the line, never a drop to zero.

use std::sync::Arc;

use gpui_kit::*;

/// How thick the line is.
const LINE_WIDTH: f32 = 1.5;
/// The side of the mark for a sample with no neighbour, which a line could not show.
const DOT_SIZE: f32 = 2.;

/// A series as runs of connected points, each `(x, y)` in `0.0..=1.0` with `y` measured upward.
///
/// The vertical scale runs from zero to the largest value, so a flat series does not look like a
/// swing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SparklineShape {
    runs: Vec<Vec<(f32, f32)>>,
}

impl SparklineShape {
    /// The shape of `values`, oldest first, with `None` for each gap.
    pub fn new(values: &[Option<u64>]) -> Self {
        let top = values.iter().flatten().copied().max().unwrap_or(0);
        let last = values.len().saturating_sub(1).max(1) as f64;
        let mut runs = Vec::new();
        let mut run = Vec::new();
        for (index, value) in values.iter().enumerate() {
            match value {
                Some(value) => {
                    let y = if top == 0 {
                        0.
                    } else {
                        *value as f64 / top as f64
                    };
                    run.push(((index as f64 / last) as f32, y as f32));
                }
                None if !run.is_empty() => runs.push(std::mem::take(&mut run)),
                None => {}
            }
        }
        if !run.is_empty() {
            runs.push(run);
        }
        Self { runs }
    }

    /// The connected runs, oldest first.
    pub fn runs(&self) -> &[Vec<(f32, f32)>] {
        &self.runs
    }

    /// Whether there is nothing to draw.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }
}

/// `shape` painted in `color`, filling the element's bounds.
pub fn sparkline(shape: Arc<SparklineShape>, color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            // Inset so that a line at either extreme is not clipped by half its width.
            let inset = LINE_WIDTH;
            let width = (f32::from(bounds.size.width) - 2. * inset).max(0.);
            let height = (f32::from(bounds.size.height) - 2. * inset).max(0.);
            let at = |(x, y): (f32, f32)| {
                point(
                    bounds.origin.x + px(inset + x * width),
                    bounds.origin.y + px(inset + (1. - y) * height),
                )
            };
            for run in shape.runs() {
                if let [only] = run.as_slice() {
                    let center = at(*only);
                    let half = px(DOT_SIZE / 2.);
                    window.paint_quad(fill(
                        Bounds::new(
                            point(center.x - half, center.y - half),
                            size(px(DOT_SIZE), px(DOT_SIZE)),
                        ),
                        color,
                    ));
                    continue;
                }
                let mut path = PathBuilder::stroke(px(LINE_WIDTH));
                for (index, sample) in run.iter().enumerate() {
                    if index == 0 {
                        path.move_to(at(*sample));
                    } else {
                        path.line_to(at(*sample));
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            }
        },
    )
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::SparklineShape;

    #[test]
    fn a_gap_splits_the_line_instead_of_dropping_to_zero() {
        let shape = SparklineShape::new(&[Some(5), Some(10), None, Some(10)]);
        assert_eq!(shape.runs().len(), 2);
        assert_eq!(shape.runs()[0], vec![(0., 0.5), (1. / 3., 1.)]);
        assert_eq!(shape.runs()[1], vec![(1., 1.)]);
    }

    #[test]
    fn the_scale_starts_at_zero() {
        let shape = SparklineShape::new(&[Some(90), Some(100)]);
        assert_eq!(shape.runs()[0], vec![(0., 0.9), (1., 1.)]);
    }

    #[test]
    fn all_zero_values_lie_on_the_baseline() {
        let shape = SparklineShape::new(&[Some(0), Some(0)]);
        assert_eq!(shape.runs()[0], vec![(0., 0.), (1., 0.)]);
    }

    #[test]
    fn no_samples_draw_nothing() {
        assert!(SparklineShape::new(&[]).is_empty());
        assert!(SparklineShape::new(&[None, None]).is_empty());
    }

    #[test]
    fn a_single_sample_stays_in_range() {
        let shape = SparklineShape::new(&[Some(7)]);
        assert_eq!(shape.runs()[0], vec![(0., 1.)]);
    }
}
