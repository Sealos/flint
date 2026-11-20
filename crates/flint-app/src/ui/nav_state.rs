//! The navigation's part of the visible view tree ([`crate::ui::view_state`]).

use super::super::view_state::{NavEntryState, NavSectionState, NavState, error_text};
use super::{Marker, NavPanel, NavRow, marker};

impl NavPanel {
    /// The sections, and the entries of the open ones. `None` with no cluster active.
    pub fn view_state(&self) -> Option<NavState> {
        let cluster = self.cluster.as_ref()?;
        let error = self.error.as_deref().map(error_text);
        let Some(discovery) = &self.discovery else {
            let status = if self.connection_failed || self.error.is_some() {
                "failed"
            } else {
                "discovering"
            };
            return Some(NavState {
                status,
                error,
                sections: Vec::new(),
            });
        };
        let collapsed = self.collapsed.get(cluster).cloned().unwrap_or_default();
        let cursor = self.cursor();
        let sections = discovery
            .navigation
            .sections
            .iter()
            .map(|section| {
                let is_collapsed = collapsed.is_collapsed(section);
                NavSectionState {
                    title: section.title.clone(),
                    collapsed: is_collapsed,
                    count: section.entries.len(),
                    cursor: cursor == Some(NavRow::Section(section.title.clone())),
                    entries: if is_collapsed {
                        Vec::new()
                    } else {
                        section
                            .entries
                            .iter()
                            .map(|entry| NavEntryState {
                                kind: entry.id.to_string(),
                                label: entry.label.clone(),
                                selected: self.selected.as_ref() == Some(&entry.id),
                                cursor: cursor == Some(NavRow::Kind(entry.id.clone())),
                                marker: self
                                    .states
                                    .get(&entry.id)
                                    .and_then(marker)
                                    .map(marker_name),
                            })
                            .collect()
                    },
                }
            })
            .collect();
        Some(NavState {
            status: "ready",
            error,
            sections,
        })
    }
}

fn marker_name(marker: Marker) -> &'static str {
    match marker {
        Marker::Loading => "loading",
        Marker::Reconnecting => "reconnecting",
        Marker::Failed => "failed",
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::{AppContext as _, TestAppContext};

    use super::{Marker, NavPanel, marker_name};

    #[test]
    fn markers_have_names() {
        assert_eq!(marker_name(Marker::Loading), "loading");
        assert_eq!(marker_name(Marker::Failed), "failed");
    }

    #[gpui_kit::test]
    fn no_cluster_no_navigation(cx: &mut TestAppContext) {
        let nav = cx.new(NavPanel::new);
        assert_eq!(nav.read_with(cx, |nav, _| nav.view_state()), None);
        nav.update(cx, |nav, cx| {
            nav.set_cluster(Some("a".into()), None, None, cx)
        });
        let state = nav.read_with(cx, |nav, _| nav.view_state()).unwrap();
        assert_eq!(state.status, "discovering");
        assert!(state.sections.is_empty());
    }
}
