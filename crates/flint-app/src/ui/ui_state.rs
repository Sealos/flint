//! UI state kept in `state.toml`: the namespace filter and the last open kind of each cluster,
//! and the pane widths.
//!
//! The file is read once at startup and written whole after each change, off the UI thread.
//! The app is its only writer.

use std::path::PathBuf;

use flint_core::settings::{self, PaneWidths, State};

pub struct UiState {
    dir: PathBuf,
    state: State,
}

impl UiState {
    /// Loads `state.toml` from `dir`. A missing file means defaults; an unreadable one is
    /// logged and treated as defaults too (the next change overwrites it).
    pub fn load(dir: PathBuf) -> Self {
        let state = match settings::load_state(&dir) {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!("using default UI state: {e}");
                State::default()
            }
        };
        UiState { dir, state }
    }

    /// The namespaces `cluster` is filtered to. Empty means all namespaces.
    pub fn namespace_filter(&self, cluster: &str) -> &[String] {
        self.state
            .namespace_filters
            .get(cluster)
            .map_or(&[], Vec::as_slice)
    }

    /// Remembers `cluster`'s namespace filter. Returns what to save, or `None` if nothing
    /// changed.
    pub fn set_namespace_filter(
        &mut self,
        cluster: &str,
        namespaces: Vec<String>,
    ) -> Option<(PathBuf, State)> {
        let namespaces = normalize(namespaces);
        if self.namespace_filter(cluster) == namespaces.as_slice() {
            return None;
        }
        if namespaces.is_empty() {
            self.state.namespace_filters.remove(cluster);
        } else {
            self.state
                .namespace_filters
                .insert(cluster.to_owned(), namespaces);
        }
        Some(self.to_save())
    }

    /// The kind (id) last open on `cluster`.
    pub fn last_view(&self, cluster: &str) -> Option<&str> {
        self.state.last_views.get(cluster).map(String::as_str)
    }

    /// Remembers the kind open on `cluster`. Returns what to save, or `None` if nothing changed.
    pub fn set_last_view(&mut self, cluster: &str, kind: &str) -> Option<(PathBuf, State)> {
        if self.last_view(cluster) == Some(kind) {
            return None;
        }
        self.state
            .last_views
            .insert(cluster.to_owned(), kind.to_owned());
        Some(self.to_save())
    }

    pub fn panes(&self) -> PaneWidths {
        self.state.panes
    }

    /// Remembers the pane widths. Returns what to save, or `None` if nothing changed.
    pub fn set_panes(&mut self, panes: PaneWidths) -> Option<(PathBuf, State)> {
        if self.state.panes == panes {
            return None;
        }
        self.state.panes = panes;
        Some(self.to_save())
    }

    fn to_save(&self) -> (PathBuf, State) {
        (self.dir.clone(), self.state.clone())
    }
}

/// Sorted, without duplicates or empty names.
pub fn normalize(mut namespaces: Vec<String>) -> Vec<String> {
    namespaces.retain(|n| !n.is_empty());
    namespaces.sort();
    namespaces.dedup();
    namespaces
}

/// Writes `state` to `dir`. Blocking; call it on a background thread.
pub fn save((dir, state): (PathBuf, State)) {
    if let Err(e) = settings::save_state(&dir, &state) {
        tracing::warn!("could not save UI state: {e}");
    }
}

/// The namespace filter after toggling `namespace` in `current`. Empty means all namespaces.
pub fn toggle_namespace(current: &[String], namespace: &str) -> Vec<String> {
    let mut next: Vec<String> = current.to_vec();
    if let Some(i) = next.iter().position(|n| n == namespace) {
        next.remove(i);
    } else {
        next.push(namespace.to_owned());
    }
    normalize(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers UP-21
    #[test]
    fn namespace_filter_is_remembered_per_cluster_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let mut ui = UiState::load(dir.path().to_owned());
        assert!(ui.namespace_filter("kind-flint-dev").is_empty());

        let save_now = ui
            .set_namespace_filter(
                "kind-flint-dev",
                vec!["demo".into(), "kube-system".into(), "demo".into()],
            )
            .expect("changed");
        save(save_now);
        assert!(
            ui.set_namespace_filter("kind-flint-dev", vec!["kube-system".into(), "demo".into()])
                .is_none(),
            "same set in another order is no change"
        );

        let reloaded = UiState::load(dir.path().to_owned());
        assert_eq!(
            reloaded.namespace_filter("kind-flint-dev"),
            ["demo", "kube-system"]
        );
        assert!(reloaded.namespace_filter("other").is_empty());

        // Back to all namespaces removes the entry.
        let mut ui = reloaded;
        save(ui.set_namespace_filter("kind-flint-dev", vec![]).unwrap());
        let reloaded = UiState::load(dir.path().to_owned());
        assert!(reloaded.namespace_filter("kind-flint-dev").is_empty());
        let text = std::fs::read_to_string(dir.path().join(settings::STATE_FILE)).unwrap();
        assert!(!text.contains("kind-flint-dev"), "{text}");
    }

    // covers UP-21
    #[test]
    fn toggling_adds_and_removes_one_namespace() {
        let one = toggle_namespace(&[], "demo");
        assert_eq!(one, ["demo"]);
        let two = toggle_namespace(&one, "default");
        assert_eq!(two, ["default", "demo"]);
        assert_eq!(toggle_namespace(&two, "demo"), ["default"]);
        assert!(toggle_namespace(&["demo".into()], "demo").is_empty());
    }

    // covers UP-24
    #[test]
    fn last_view_and_pane_widths_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let mut ui = UiState::load(dir.path().to_owned());
        assert_eq!(ui.last_view("kind-flint-dev"), None);
        save(
            ui.set_last_view("kind-flint-dev", "deployments.apps")
                .unwrap(),
        );
        assert!(
            ui.set_last_view("kind-flint-dev", "deployments.apps")
                .is_none()
        );
        let panes = PaneWidths {
            clusters: Some(200),
            navigation: Some(260),
            detail: None,
        };
        save(ui.set_panes(panes).unwrap());
        assert!(ui.set_panes(panes).is_none());

        let reloaded = UiState::load(dir.path().to_owned());
        assert_eq!(
            reloaded.last_view("kind-flint-dev"),
            Some("deployments.apps")
        );
        assert_eq!(reloaded.last_view("other"), None);
        assert_eq!(reloaded.panes(), panes);
    }

    #[test]
    fn unreadable_state_falls_back_to_defaults() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(settings::STATE_FILE), "not = [valid").unwrap();
        let ui = UiState::load(dir.path().to_owned());
        assert!(ui.namespace_filter("x").is_empty());
    }
}
