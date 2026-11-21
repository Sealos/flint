//! The detail pane's part of the visible view tree ([`crate::ui::view_state`]). It names the
//! object and the pane's state, never the object's content: Secret values stay out.

use gpui_kit::*;

use super::super::view_state::{Banner, DetailState, Level, SectionCount, error_text};
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
        if let Some(waiting) = self.waiting() {
            banners.push(Banner::new("detail-pending", Level::Info, waiting));
        }
        if self.shows_stale_notice() {
            // Only the server's words are reduced, so a long error cannot cut off the time.
            let text = format!(
                "{} {}",
                self.freshness.headline_with(error_text),
                super::sync::last_updated(self.fetched_at)
            );
            banners.push(Banner::new("detail-stale", Level::Warning, text));
        }
        if let Some(error) = &self.refresh_error {
            banners.push(Banner::new(
                "detail-refresh-error",
                Level::Danger,
                error_text(error),
            ));
        }
        let sections = match &self.fetch {
            Fetch::Loaded { detail, overview } => {
                let mut sections = vec![
                    ("Labels", overview.labels.len()),
                    ("Annotations", overview.annotations.len()),
                    ("Owners", overview.owners.len()),
                    ("Conditions", overview.conditions.len()),
                    ("Events", detail.events.len()),
                ];
                if let Some(secret) = &overview.secret {
                    sections.push(("Data", secret.entries.len()));
                }
                sections
                    .into_iter()
                    .map(|(name, count)| SectionCount { name, count })
                    .collect()
            }
            _ => Vec::new(),
        };
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
            sections,
            revealed,
            banners,
            // Filled in by the workspace, which knows the cluster's state.
            actions: Vec::new(),
            edit: self.edit.as_ref().map(|edit| edit.phase.name()),
            edit_error: self
                .edit
                .as_ref()
                .and_then(|edit| edit.error.as_deref())
                .map(error_text),
            edit_blocked: self.edit_blocked().map(|why| error_text(&why)),
        }
    }
}

impl DetailView {
    /// Whether focus is in the YAML editor.
    pub fn yaml_focused(&self, window: &Window, cx: &App) -> bool {
        self.yaml.focus_handle(cx).contains_focused(window, cx)
            || self
                .edit
                .as_ref()
                .is_some_and(|edit| edit.editor.focus_handle(cx).contains_focused(window, cx))
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
