use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{Bounds, Pixels, Point, Styled as _, point, px};

pub(super) const SHOW_DELAY: Duration = Duration::from_millis(700);
const MAX_WIDTH: Pixels = px(280.);
const PATH_TAIL_COMPONENTS: usize = 3;

pub(super) fn cap_width(tooltip: Tooltip) -> Tooltip {
    tooltip.max_w(MAX_WIDTH).whitespace_normal()
}

/// The gap between a row's edge and a tooltip placed beside it.
const BESIDE_GAP: Pixels = px(8.);

/// Margins (left, top) that move a tooltip, which the window draws just below and right of the
/// pointer, to the right of `row` and level with its top, so it covers none of the rows around it.
pub(super) fn beside_row_margins(row: Bounds<Pixels>, pointer: Point<Pixels>) -> Point<Pixels> {
    // The window puts the tooltip's origin one pixel off the pointer.
    let origin = pointer + point(px(1.), px(1.));
    point(
        (row.right() + BESIDE_GAP - origin.x).max(Pixels::ZERO),
        row.top() - origin.y,
    )
}

/// A tooltip placed beside `row` instead of under the pointer.
pub(super) fn beside_row(tooltip: Tooltip, row: Bounds<Pixels>, pointer: Point<Pixels>) -> Tooltip {
    let margins = beside_row_margins(row, pointer);
    cap_width(tooltip).ml(margins.x).mt(margins.y)
}

pub(super) fn shorten_path(path: &Path, home: Option<&Path>) -> String {
    let relative = home.and_then(|home| path.strip_prefix(home).ok());
    let mut components = relative.unwrap_or(path).components().rev();
    let tail: Vec<_> = components.by_ref().take(PATH_TAIL_COMPONENTS).collect();
    if components.any(|part| !matches!(part, Component::RootDir | Component::Prefix(_))) {
        let mut shortened = if relative.is_some() {
            Path::new("~").join("…")
        } else {
            PathBuf::from("…")
        };
        shortened.extend(tail.into_iter().rev().map(|part| part.as_os_str()));
        shortened.display().to_string()
    } else {
        match relative {
            Some(relative) if relative.as_os_str().is_empty() => "~".into(),
            Some(relative) => Path::new("~").join(relative).display().to_string(),
            None => path.display().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_paths_preserve_their_components_and_abbreviate_home() {
        let home = Path::new("/Users/sam");
        for (path, expected) in [
            ("/Users/sam", "~"),
            ("/Users/sam/.kube/config", "~/.kube/config"),
            ("/Users/samuel/config", "/Users/samuel/config"),
            ("configs/team/config", "configs/team/config"),
        ] {
            assert_eq!(
                Path::new(&shorten_path(Path::new(path), Some(home))),
                Path::new(expected)
            );
        }
    }

    #[test]
    fn long_paths_keep_the_last_three_components() {
        let home = Path::new("/Users/sam");
        for (path, home, expected) in [
            (
                "/Users/sam/work/configs/team/prod/kube.yaml",
                Some(home),
                "~/…/team/prod/kube.yaml",
            ),
            (
                "/etc/kube/configs/team/prod/kube.yaml",
                Some(home),
                "…/team/prod/kube.yaml",
            ),
            (
                "work/configs/team/prod/kube.yaml",
                None,
                "…/team/prod/kube.yaml",
            ),
        ] {
            assert_eq!(
                Path::new(&shorten_path(Path::new(path), home)),
                Path::new(expected)
            );
        }
    }

    #[test]
    fn a_tooltip_beside_a_row_clears_the_rows_around_it() {
        let row = Bounds::new(point(px(0.), px(100.)), gpui_kit::size(px(200.), px(24.)));
        let pointer = point(px(50.), px(110.));
        let margins = beside_row_margins(row, pointer);
        // Drawn at the pointer plus one pixel, plus the margins.
        let left = pointer.x + px(1.) + margins.x;
        let top = pointer.y + px(1.) + margins.y;
        assert!(left >= row.right());
        assert_eq!(top, row.top());
        // A pointer already beyond the row never moves the tooltip left.
        let far = beside_row_margins(row, point(px(400.), px(110.)));
        assert_eq!(far.x, Pixels::ZERO);
    }

    #[test]
    fn capped_tooltips_allow_text_to_wrap() {
        let mut tooltip = cap_width(Tooltip::new("Tooltip text"));
        let style = tooltip.style();
        assert_eq!(style.max_size.width, Some(MAX_WIDTH.into()));
        assert_eq!(style.text.white_space, Some(gpui_kit::WhiteSpace::Normal));
    }
}
