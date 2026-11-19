//! The detail pane's part of the visible view tree ([`crate::ui::view_state`]). It names the
//! object and the pane's state, never the object's content: Secret values stay out.

use gpui_kit::*;

use super::super::view_state::{Banner, DetailState, Level, error_text};
use super::{DetailTab, DetailView, Fetch};

impl DetailView {
    pub fn view_state(&self) -> DetailState {
        let (fetch, error) = match &self.fetch {
            Fetch::Loading => ("loading", None),
            Fetch::Loaded { .. } => ("loaded", None),
            Fetch::Failed(error) => ("failed", Some(error_text(error))),
        };
        let mut banners = Vec::new();
        if self.gone {
            banners.push(Banner::new("detail-gone", Level::Warning, "Deleted"));
        }
        if let Some(error) = &self.refresh_error {
            banners.push(Banner::new(
                "detail-refresh-error",
                Level::Danger,
                error_text(error),
            ));
        }
        let mut revealed: Vec<String> = self.revealed.iter().cloned().collect();
        revealed.sort_unstable();
        DetailState {
            kind: self.object.kind.to_string(),
            namespace: self.object.namespace.clone(),
            name: self.object.name.clone(),
            tab: tab_name(self.tab),
            fetch,
            error,
            fetching: self.fetching,
            labels_open: self.show_labels,
            annotations_open: self.show_annotations,
            managed_fields_shown: self.show_managed_fields,
            revealed,
            banners,
        }
    }
}

impl DetailView {
    /// Whether focus is in the YAML editor.
    pub fn yaml_focused(&self, window: &Window, cx: &App) -> bool {
        self.yaml.focus_handle(cx).contains_focused(window, cx)
    }
}

fn tab_name(tab: DetailTab) -> &'static str {
    match tab {
        DetailTab::Overview => "overview",
        DetailTab::Yaml => "yaml",
    }
}

#[cfg(test)]
mod tests {
    use super::{DetailTab, tab_name};

    #[test]
    fn tabs_are_named_like_show_tab_takes_them() {
        assert_eq!(tab_name(DetailTab::Overview), "overview");
        assert_eq!(tab_name(DetailTab::Yaml), "yaml");
    }
}
