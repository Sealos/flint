use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{Pixels, Styled as _, px};

pub(super) const SHOW_DELAY: Duration = Duration::from_millis(700);
const MAX_WIDTH: Pixels = px(280.);
const PATH_TAIL_COMPONENTS: usize = 3;

pub(super) fn cap_width(tooltip: Tooltip) -> Tooltip {
    tooltip.max_w(MAX_WIDTH).whitespace_normal()
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
    fn capped_tooltips_allow_text_to_wrap() {
        let mut tooltip = cap_width(Tooltip::new("Tooltip text"));
        let style = tooltip.style();
        assert_eq!(style.max_size.width, Some(MAX_WIDTH.into()));
        assert_eq!(style.text.white_space, Some(gpui_kit::WhiteSpace::Normal));
    }
}
